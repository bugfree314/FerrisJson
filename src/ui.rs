//! Shared gpui widgets and JSON syntax highlighting.
use crate::palette::{color, Palette};
use gpui::*;
use std::ops::Range;

/// The bundled monospace family for JSON text. The platform text system
/// supplies script fallbacks for characters this face does not cover.
pub fn mono_family() -> SharedString {
    SharedString::from(if cfg!(target_os = "windows") { "Consolas" } else { "Menlo" })
}

pub fn mono_font() -> Font {
    font(mono_family())
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
            .build()
            .ok()?;
        Some(Self { signature: (term.to_owned(), case, regex, scope), matcher })
    }

    fn includes(&self, key: bool) -> bool {
        use crate::document::SearchScope;
        match self.signature.3 {
            SearchScope::All => true,
            SearchScope::Keys => key,
            SearchScope::Values => !key,
        }
    }

    pub fn runs(&self, text: &str, foreground: Rgba, palette: Palette, key: bool) -> Vec<TextRun> {
        let mut runs = Vec::new();
        self.append(&mut runs, text, foreground, palette, key, false);
        runs
    }

    fn append(
        &self,
        runs: &mut Vec<TextRun>,
        text: &str,
        foreground: Rgba,
        palette: Palette,
        key: bool,
        quoted: bool,
    ) {
        if !self.includes(key) {
            runs.push(run(text.len(), foreground, None));
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
                    } else {
                        2
                    };
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
            } else {
                (found.start(), found.end())
            };
            runs.push(run(start - end, foreground, None));
            runs.push(run(finish - start, foreground, Some(color(palette.search_match))));
            end = finish;
        }
        runs.push(run(text.len() - end, foreground, None));
    }
}

fn run(len: usize, foreground: Rgba, background: Option<Rgba>) -> TextRun {
    TextRun {
        len,
        font: mono_font(),
        color: foreground.into(),
        background_color: background.map(Into::into),
        underline: None,
        strikethrough: None,
    }
}

pub fn text_runs(text: &str, foreground: Rgba) -> Vec<TextRun> {
    vec![run(text.len(), foreground, None)]
}

pub fn json_runs(text: &str, palette: Palette, highlight: Option<&SearchHighlight>) -> Vec<TextRun> {
    let mut runs = Vec::new();
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
        let foreground = palette.kind(kind);
        let quoted = bytes[start] == b'"';
        let key = quoted && text[i..].trim_start().starts_with(':');
        if let Some(highlight) = highlight.filter(|_| quoted || matches!(kind, "number" | "boolean" | "null")) {
            highlight.append(&mut runs, &text[start..i], foreground, palette, key, quoted);
        } else {
            runs.push(run(i - start, foreground, None));
        }
    }
    runs
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

pub fn json_text(text: &str, palette: Palette, highlight: Option<&SearchHighlight>) -> StyledText {
    StyledText::new(text.to_owned()).with_runs(json_runs(text, palette, highlight))
}

pub fn plain_text(text: &str, foreground: Rgba) -> StyledText {
    StyledText::new(text.to_owned()).with_runs(text_runs(text, foreground))
}

/// Clickable toolbar button. Callers attach the click handler themselves; the id
/// is assigned here because gpui only tracks clicks for identified elements.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(22.))
        .px_2()
        .rounded(px(4.))
        .bg(color(palette.stripe))
        .border_1()
        .border_color(color(palette.selection))
        .text_sm()
        .text_color(color(palette.punctuation))
        .cursor_pointer()
        .hover(|style| style.bg(color(palette.selection)))
        .child(label.into())
}

pub fn toggle(id: impl Into<ElementId>, label: &str, checked: bool, palette: Palette) -> Stateful<Div> {
    button(id, format!("{} {label}", if checked { "☑" } else { "☐" }), palette)
}

pub fn field(input: &Entity<TextInput>, width: f32, palette: Palette) -> Div {
    div()
        .w(px(width))
        .h(px(26.))
        .px_1()
        .flex()
        .items_center()
        .bg(color(palette.background))
        .border_1()
        .border_color(color(palette.selection))
        .rounded(px(4.))
        .child(input.clone())
}

pub fn modal(id: &'static str, palette: Palette, width: f32, body: impl IntoElement) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000aa))
        .occlude()
        .child(
            div()
                .id("modal-panel")
                .w(px(width))
                .max_h(px(700.))
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded(px(8.))
                .border_1()
                .border_color(color(palette.selection))
                .bg(color(palette.background))
                .text_color(color(palette.punctuation))
                .overflow_y_scroll()
                .child(body),
        )
}

// ---------------------------------------------------------------------------
// Single-line text input
// ---------------------------------------------------------------------------

actions!(
    text_input,
    [
        Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste, Cut,
        Copy, Submit, Escape,
    ]
);

/// Register the editing bindings for [`TextInput`]. The bindings are scoped to
/// the `TextInput` key context so the viewer keeps Vim navigation elsewhere.
pub fn bind_text_input_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
        KeyBinding::new("secondary-v", Paste, Some("TextInput")),
        KeyBinding::new("secondary-c", Copy, Some("TextInput")),
        KeyBinding::new("secondary-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        KeyBinding::new("escape", Escape, Some("TextInput")),
    ]);
}

#[derive(Clone, Debug)]
pub enum InputEvent {
    Changed(SharedString),
    Submitted,
    Escape,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    limit: usize,
}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, limit: usize, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            limit,
        }
    }

    /// Replace the whole value programmatically, for example when selecting a
    /// different color in the editor.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let mut text: String = text.into();
        if text.len() > self.limit {
            let mut end = self.limit;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.content.len();
        self.move_to(end, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace(['\n', '\r'], " ");
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submitted);
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Escape);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_offset(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn clamp_offset(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.content.len());
        while !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_offset(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..self.clamp_offset(offset)]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_offset(offset);
        self.content[offset..]
            .char_indices()
            .nth(1)
            .map_or(self.content.len(), |(index, _)| offset + index)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let mut new_text = new_text.to_owned().replace(['\n', '\r'], " ");
        let room = self.limit.saturating_sub(self.content.len().saturating_sub(range.len()));
        if new_text.len() > room {
            let mut end = room;
            while !new_text.is_char_boundary(end) {
                end -= 1;
            }
            new_text.truncate(end);
        }
        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        let end = range.start + new_text.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.emit(InputEvent::Changed(self.content.clone()));
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        if !new_text.is_empty() {
            let end = self.selected_range.end;
            self.marked_range = Some(end.saturating_sub(new_text.len())..end);
        }
        if let Some(range_utf16) = new_selected_range_utf16 {
            self.selected_range = self.range_from_utf16(&range_utf16);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(bounds.left() + last_layout.x_for_index(range.start), bounds.top()),
            point(bounds.left() + last_layout.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), rgb(0x6b7280).into())
        } else {
            (content, style.color)
        };
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun { len: marked_range.start, ..run.clone() },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun { len: display_text.len() - marked_range.end, ..run },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);
        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    rgb(0x66b2ff),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(selected_range.start), bounds.top()),
                        point(bounds.left() + line.x_for_index(selected_range.end), bounds.bottom()),
                    ),
                    rgba(0x3399ff40),
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().unwrap();
        line.paint(bounds.origin, window.line_height(), window, cx).ok();
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(24.))
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .font_family(mono_family())
            .text_size(px(13.))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<InputEvent> for TextInput {}

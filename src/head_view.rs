//! Read-only first-paint preview of a JSON file's opening content.
//!
//! `read_head` reads a bounded prefix of the file directly (no snapshot copy,
//! no index) and scans it with a tolerant bounded parser to produce the root
//! kind plus the first top-level entries. It is a display-only approximation:
//! values that are not closed inside the read block are marked as such, and a
//! syntax problem stops the scan with a note instead of failing the open flow.
//! The authoritative snapshot, validation and index still come from the
//! background load; the head view is discarded once that finishes.

use std::{fs::OpenOptions, io::Read, path::Path};

/// First read attempt; small enough to stay interactive on slow disks.
const HEAD_BLOCK: usize = 64 * 1024;
/// Retry limit when no complete entry fits the first block (giant first value).
const HEAD_BLOCK_MAX: usize = 1024 * 1024;
/// Matches the sparse engine's published page size.
const MAX_ENTRIES: usize = 64;
/// Matches the sparse engine's preview budget per value.
const PREVIEW_BYTES: usize = 160;
/// Display cap for object keys, matching the row label cache.
const KEY_CHARS: usize = 160;
/// Raw text fallback when no complete entry fits the read block.
const FRAGMENT_BYTES: usize = 2 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Array,
    Object,
    String,
    Number,
    Boolean,
    Null,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Array => "array",
            Kind::Object => "object",
            Kind::String => "string",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            Kind::Null => "null",
        }
    }
    fn container(self) -> bool {
        matches!(self, Kind::Array | Kind::Object)
    }
}

pub struct Entry {
    /// `[index]` for arrays, the raw key text for objects.
    pub label: String,
    /// Raw head text of the value; lossy UTF-8, truncated with `…`.
    pub preview: String,
    pub kind: Kind,
    /// The value's end was found inside the read block.
    pub closed: bool,
}

pub struct HeadView {
    pub root_kind: Kind,
    pub entries: Vec<Entry>,
    /// Raw text fallback when no complete entry fits the read block.
    pub fragment: Option<String>,
    /// The preview does not cover the complete document.
    pub truncated: bool,
    /// The bounded scan stopped on unexpected bytes; the full load decides.
    pub parse_note: Option<String>,
    pub bytes_read: u64,
    pub total_bytes: u64,
}

/// Read a bounded prefix of `path` and scan it for the first paint.
///
/// The file is opened with read sharing only; no snapshot is created. The
/// source fingerprint (size and mtime) is checked across the read so a head
/// view is never shown for a file that changed while it was being read.
pub fn read_head(path: &Path) -> Result<HeadView, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    let total = before.len();
    let mut data = Vec::with_capacity(HEAD_BLOCK);
    read_up_to(&mut file, &mut data, HEAD_BLOCK)?;
    let mut view = parse(&data, (data.len() as u64) < total)?;
    if view.entries.is_empty()
        && view.root_kind.container()
        && data.len() < HEAD_BLOCK_MAX
        && (data.len() as u64) < total
    {
        // Giant first value: retry once with a larger bounded block instead
        // of leaving the user with no structure at all.
        read_up_to(&mut file, &mut data, HEAD_BLOCK_MAX)?;
        view = parse(&data, (data.len() as u64) < total)?;
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("Source changed while reading the head block".into());
    }
    view.bytes_read = data.len() as u64;
    view.total_bytes = total;
    Ok(view)
}

fn read_up_to(file: &mut std::fs::File, data: &mut Vec<u8>, target: usize) -> Result<(), String> {
    data.reserve(target.saturating_sub(data.len()));
    let mut chunk = [0u8; 64 * 1024];
    while data.len() < target {
        let want = (target - data.len()).min(chunk.len());
        let count = file.read(&mut chunk[..want]).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..count]);
    }
    Ok(())
}

fn parse(data: &[u8], file_continues: bool) -> Result<HeadView, String> {
    let mut start = 0;
    if data.starts_with(&[0xef, 0xbb, 0xbf]) {
        start = 3;
    }
    let start = skip_ws(data, start);
    let first = *data.get(start).ok_or("Empty head block")?;
    let root_kind = match first {
        b'[' => Kind::Array,
        b'{' => Kind::Object,
        b'"' => Kind::String,
        b'-' | b'0'..=b'9' => Kind::Number,
        b't' | b'f' => Kind::Boolean,
        b'n' => Kind::Null,
        other => return Err(format!("Unexpected byte {other:#x} at the start of the file")),
    };
    let scan = match root_kind {
        Kind::Array => parse_array(data, start, file_continues),
        Kind::Object => parse_object(data, start, file_continues),
        _ => parse_scalar(data, start, root_kind, file_continues),
    };
    let mut view = HeadView {
        root_kind,
        entries: scan.entries,
        fragment: None,
        truncated: scan.truncated,
        parse_note: scan.note,
        bytes_read: 0,
        total_bytes: 0,
    };
    if view.entries.is_empty() && view.truncated {
        view.fragment = Some(fragment_text(data));
    }
    Ok(view)
}

struct Scan {
    entries: Vec<Entry>,
    truncated: bool,
    note: Option<String>,
}

fn parse_array(data: &[u8], start: usize, file_continues: bool) -> Scan {
    let mut entries = Vec::new();
    let truncated;
    let mut note = None;
    let mut pos = skip_ws(data, start + 1);
    loop {
        match data.get(pos) {
            None => {
                truncated = file_continues;
                break;
            }
            Some(b']') => {
                truncated = file_continues;
                break;
            }
            _ => {}
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let value_start = pos;
        match scan_value(data, pos, file_continues) {
            Ok(value) => {
                entries.push(Entry {
                    label: format!("[{}]", entries.len()),
                    preview: head_text(data, value_start, value.end, !value.closed),
                    kind: value.kind,
                    closed: value.closed,
                });
                pos = skip_ws(data, value.end);
                match data.get(pos) {
                    Some(b',') => pos = skip_ws(data, pos + 1),
                    Some(b']') => {
                        truncated = file_continues;
                        break;
                    }
                    None => {
                        truncated = file_continues;
                        break;
                    }
                    _ => {
                        note = Some(format!("Unexpected byte after item {}", entries.len()));
                        truncated = file_continues;
                        break;
                    }
                }
            }
            Err(error) => {
                note = Some(error);
                truncated = file_continues;
                break;
            }
        }
    }
    Scan {
        entries,
        truncated,
        note,
    }
}

fn parse_object(data: &[u8], start: usize, file_continues: bool) -> Scan {
    let mut entries = Vec::new();
    let truncated;
    let mut note = None;
    let mut pos = skip_ws(data, start + 1);
    loop {
        match data.get(pos) {
            None => {
                truncated = file_continues;
                break;
            }
            Some(b'}') => {
                truncated = file_continues;
                break;
            }
            _ => {}
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        if data.get(pos) != Some(&b'"') {
            note = Some("Expected an object key".into());
            truncated = file_continues;
            break;
        }
        let Some(key_end) = scan_string(data, pos) else {
            truncated = file_continues;
            break;
        };
        let key = key_text(data, pos + 1, key_end - 1);
        pos = skip_ws(data, key_end);
        if data.get(pos) != Some(&b':') {
            note = Some("Expected ':' after the object key".into());
            truncated = file_continues;
            break;
        }
        pos = skip_ws(data, pos + 1);
        let value_start = pos;
        match scan_value(data, pos, file_continues) {
            Ok(value) => {
                entries.push(Entry {
                    label: key,
                    preview: head_text(data, value_start, value.end, !value.closed),
                    kind: value.kind,
                    closed: value.closed,
                });
                pos = skip_ws(data, value.end);
                match data.get(pos) {
                    Some(b',') => pos = skip_ws(data, pos + 1),
                    Some(b'}') => {
                        truncated = file_continues;
                        break;
                    }
                    None => {
                        truncated = file_continues;
                        break;
                    }
                    _ => {
                        note = Some(format!("Unexpected byte after field {}", entries.len()));
                        truncated = file_continues;
                        break;
                    }
                }
            }
            Err(error) => {
                note = Some(error);
                truncated = file_continues;
                break;
            }
        }
    }
    Scan {
        entries,
        truncated,
        note,
    }
}

fn parse_scalar(data: &[u8], start: usize, kind: Kind, file_continues: bool) -> Scan {
    match scan_value(data, start, file_continues) {
        Ok(value) => Scan {
            entries: vec![Entry {
                label: kind.name().into(),
                preview: head_text(data, start, value.end, !value.closed),
                kind,
                closed: value.closed,
            }],
            truncated: !value.closed || file_continues,
            note: None,
        },
        Err(error) => Scan {
            entries: Vec::new(),
            truncated: true,
            note: Some(error),
        },
    }
}

struct Scanned {
    kind: Kind,
    /// One past the last byte of the value inside the block.
    end: usize,
    closed: bool,
}

fn scan_value(data: &[u8], start: usize, file_continues: bool) -> Result<Scanned, String> {
    let Some(&first) = data.get(start) else {
        return Err("No value at the current position".into());
    };
    match first {
        b'"' => match scan_string(data, start) {
            Some(end) => Ok(Scanned {
                kind: Kind::String,
                end,
                closed: true,
            }),
            None => Ok(Scanned {
                kind: Kind::String,
                end: data.len(),
                closed: false,
            }),
        },
        b'[' | b'{' => {
            let (kind, open, close) = if first == b'[' {
                (Kind::Array, b'[', b']')
            } else {
                (Kind::Object, b'{', b'}')
            };
            let mut pos = start + 1;
            let mut depth = 1usize;
            let mut closed = false;
            'scan: while let Some(&byte) = data.get(pos) {
                match byte {
                    b'"' => match scan_string(data, pos) {
                        Some(end) => pos = end - 1,
                        None => break 'scan,
                    },
                    _ if byte == open => depth += 1,
                    _ if byte == close => {
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break 'scan;
                        }
                    }
                    _ => {}
                }
                pos += 1;
            }
            let end = if closed { pos + 1 } else { data.len() };
            Ok(Scanned { kind, end, closed })
        }
        b'-' | b'+' | b'.' | b'0'..=b'9' => {
            let mut pos = start;
            while let Some(&byte) = data.get(pos) {
                if matches!(
                    byte,
                    b'-' | b'+' | b'.' | b'0'..=b'9' | b'e' | b'E'
                ) {
                    pos += 1;
                } else {
                    break;
                }
            }
            let closed = pos < data.len() || !file_continues;
            Ok(Scanned {
                kind: Kind::Number,
                end: pos,
                closed,
            })
        }
        b't' | b'f' | b'n' => {
            let (word, kind) = match first {
                b't' => (b"true" as &[u8], Kind::Boolean),
                b'f' => (b"false" as &[u8], Kind::Boolean),
                _ => (b"null" as &[u8], Kind::Null),
            };
            let available = &data[start..];
            if available.len() < word.len() {
                if word.starts_with(available) {
                    // The literal may continue in the unread part of the file.
                    return Ok(Scanned {
                        kind,
                        end: data.len(),
                        closed: false,
                    });
                }
                return Err(format!("Unexpected value at byte {start}"));
            }
            if !available.starts_with(word) {
                return Err(format!("Unexpected value at byte {start}"));
            }
            let end = start + word.len();
            let closed = end < data.len() || !file_continues;
            Ok(Scanned { kind, end, closed })
        }
        other => Err(format!("Unexpected byte {other:#x} at byte {start}")),
    }
}

/// End position (exclusive) of the string starting at `start`, or `None` when
/// the closing quote lies outside the read block. Escapes are skipped.
fn scan_string(data: &[u8], start: usize) -> Option<usize> {
    let mut pos = start + 1;
    while let Some(&byte) = data.get(pos) {
        match byte {
            b'\\' => pos += 1,
            b'"' => return Some(pos + 1),
            _ => {}
        }
        pos += 1;
    }
    None
}

fn skip_ws(data: &[u8], mut pos: usize) -> usize {
    while let Some(&byte) = data.get(pos) {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
            pos += 1;
        } else {
            break;
        }
    }
    pos
}

fn head_text(data: &[u8], start: usize, end: usize, unclosed: bool) -> String {
    let stop = end.min(start.saturating_add(PREVIEW_BYTES));
    let mut text = String::from_utf8_lossy(&data[start..stop]).into_owned();
    if (end - start > stop - start || unclosed) && !text.ends_with('…') {
        text.push('…');
    }
    text
}

fn key_text(data: &[u8], start: usize, end: usize) -> String {
    // Raw escaped content; decoding escapes is unnecessary for a label.
    let mut text = String::from_utf8_lossy(&data[start..end.max(start)]).into_owned();
    if text.chars().count() > KEY_CHARS {
        text = text.chars().take(KEY_CHARS).collect();
        text.push('…');
    }
    text
}

fn fragment_text(data: &[u8]) -> String {
    let mut text =
        String::from_utf8_lossy(&data[..data.len().min(FRAGMENT_BYTES)]).into_owned();
    text.push('…');
    text
}

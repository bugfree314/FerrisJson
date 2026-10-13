//! Search the validated source directly; avoid materializing container pages.
use super::{BLOCK, Details, Range, Reader, SUBTREE_LIMIT, SearchScope, Store};
use regex::{Regex, RegexBuilder};
struct Walk<'a, C, E> {
    store: &'a Store,
    reader: Reader<'a>,
    matcher: Regex,
    term: &'a str,
    regex: bool,
    scope: SearchScope,
    from: u32,
    id: u32,
    cancel: &'a C,
    emit: E,
}
impl<C: Fn() -> bool, E: FnMut(Option<u32>, usize, usize) -> Result<bool, String>> Walk<'_, C, E> {
    fn check(&self) -> Result<(), String> {
        if (self.cancel)() {
            Err("Search cancelled".into())
        } else {
            Ok(())
        }
    }
    fn space(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b' ' | b'\r' | b'\n' | b'\t')) {
            self.reader.next()?;
            if self.reader.pos & 4095 == 0 {
                self.check()?;
            }
        }
        Ok(())
    }
    fn text(&mut self, collect: bool) -> Result<String, String> {
        self.reader.expect(b'"')?;
        let mut text = String::new();
        while let Some(c) = self.reader.character()? {
            if collect {
                text.push(c);
            }
            if self.reader.pos & 4095 < 4 {
                self.check()?;
            }
        }
        Ok(text)
    }
    fn skip_text(&mut self) -> Result<(), String> {
        self.reader.expect(b'"')?;
        loop {
            match self.reader.next()? {
                b'"' => return Ok(()),
                b'\\' => {
                    self.reader.next()?;
                }
                _ => {}
            }
            if self.reader.pos & 4095 == 0 {
                self.check()?;
            }
        }
    }
    fn value(&mut self, key: Option<&str>) -> Result<bool, String> {
        self.check()?;
        self.space()?;
        let start = self.reader.pos;
        let id = self.id;
        let meta = self.store.sparse.get(&start).cloned();
        if let Some(meta) = &meta
            && id + meta.span.nodes <= self.from
        {
            self.id += meta.span.nodes;
            self.reader.pos = meta.span.range.end;
            return Ok(true);
        }
        self.id += 1;
        let eligible = id >= self.from;
        let key_match = eligible
            && self.scope != SearchScope::Values
            && key.is_some_and(|key| self.matcher.is_match(key));
        if key_match && !(self.emit)(Some(id), self.id as usize, self.store.node_count())? {
            return Ok(false);
        }
        if id.is_multiple_of(4096) && !(self.emit)(None, self.id as usize, self.store.node_count())?
        {
            return Ok(false);
        }
        let wanted = eligible && !key_match && self.scope != SearchScope::Keys;
        let value_match = match self.reader.peek()? {
            Some(b'{') => {
                if let Some(meta) = &meta
                    && meta.duplicate
                {
                    if let Details::Object(fields) = &meta.details {
                        for field in fields {
                            self.reader.pos = field.span.range.start;
                            if !self.value(Some(&field.name))? {
                                return Ok(false);
                            }
                        }
                        self.reader.pos = meta.span.range.end;
                    }
                } else {
                    self.reader.next()?;
                    self.space()?;
                    if self.reader.peek()? != Some(b'}') {
                        loop {
                            let key = self.text(true)?;
                            self.space()?;
                            self.reader.expect(b':')?;
                            if !self.value(Some(&key))? {
                                return Ok(false);
                            }
                            self.space()?;
                            if self.reader.peek()? != Some(b',') {
                                break;
                            }
                            self.reader.next()?;
                            self.space()?;
                        }
                    }
                    self.reader.expect(b'}')?;
                }
                false
            }
            Some(b'[') => {
                self.reader.next()?;
                self.space()?;
                let mut ordinal = 0;
                if self.from > id + 1
                    && let Some(meta) = &meta
                    && let Details::Array(anchors) = &meta.details
                {
                    let index = anchors
                        .partition_point(|a| a.prefix <= self.from - id)
                        .saturating_sub(1);
                    if let Some(anchor) = anchors.get(index) {
                        self.reader.pos = anchor.offset;
                        self.id = id + anchor.prefix;
                        ordinal = anchor.ordinal;
                    }
                }
                let numeric_keys = self.regex || self.term.bytes().all(|b| b.is_ascii_digit());
                if self.reader.peek()? != Some(b']') {
                    loop {
                        let key = if numeric_keys && self.scope != SearchScope::Values {
                            Some(ordinal.to_string())
                        } else {
                            None
                        };
                        if !self.value(key.as_deref())? {
                            return Ok(false);
                        }
                        ordinal += 1;
                        self.space()?;
                        if self.reader.peek()? != Some(b',') {
                            break;
                        }
                        self.reader.next()?;
                        self.space()?;
                    }
                }
                self.reader.expect(b']')?;
                false
            }
            Some(b'"') => {
                if !wanted {
                    if let Some(meta) = &meta {
                        self.reader.pos = meta.span.range.end;
                    } else {
                        self.skip_text()?;
                    }
                    false
                } else if self.regex {
                    if meta
                        .as_ref()
                        .is_some_and(|m| m.span.range.end - start > SUBTREE_LIMIT as u64)
                    {
                        return Err("On-demand regex strings exceed 256 KiB; narrow the scope or reopen in memory mode".into());
                    }
                    self.reader.expect(b'"')?;
                    let mut text = String::new();
                    while let Some(c) = self.reader.character()? {
                        if self.reader.pos - start > SUBTREE_LIMIT as u64 {
                            return Err("On-demand regex strings exceed 256 KiB".into());
                        }
                        text.push(c);
                        if self.reader.pos & 4095 < 4 {
                            self.check()?;
                        }
                    }
                    self.matcher.is_match(&text)
                } else {
                    self.reader.expect(b'"')?;
                    let mut window = String::new();
                    let mut added = 0;
                    let overlap = self.term.chars().count() * 4;
                    let mut found = false;
                    while let Some(c) = self.reader.character()? {
                        window.push(c);
                        added += c.len_utf8();
                        if added >= BLOCK {
                            self.check()?;
                            if self.matcher.is_match(&window) {
                                found = true;
                                break;
                            }
                            let mut offset = window.len().saturating_sub(overlap);
                            while !window.is_char_boundary(offset) {
                                offset -= 1;
                            }
                            window.drain(..offset);
                            added = 0;
                        }
                    }
                    let matched = found || self.matcher.is_match(&window);
                    if found {
                        if let Some(meta) = &meta {
                            self.reader.pos = meta.span.range.end;
                        } else {
                            while self.reader.character()?.is_some() {
                                if self.reader.pos & 4095 < 4 {
                                    self.check()?;
                                }
                            }
                        }
                    }
                    matched
                }
            }
            Some(b't' | b'f' | b'n') => {
                let text = match self.reader.peek()? {
                    Some(b't') => "true",
                    Some(b'f') => "false",
                    _ => "null",
                };
                for byte in text.bytes() {
                    self.reader.expect(byte)?;
                }
                wanted && self.matcher.is_match(text)
            }
            Some(b'-' | b'0'..=b'9') => {
                while matches!(
                    self.reader.peek()?,
                    Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                ) {
                    self.reader.next()?;
                    if self.reader.pos & 4095 == 0 {
                        self.check()?;
                    }
                }
                let end = self.reader.pos;
                let matched = if wanted {
                    self.matcher.is_match(
                        self.reader
                            .scalar(Range { start, end })?
                            .as_number()
                            .unwrap()
                            .as_str(),
                    )
                } else {
                    false
                };
                self.reader.pos = end;
                matched
            }
            _ => return Err(self.reader.error("unexpected token in validated source")),
        };
        if value_match && !(self.emit)(Some(id), self.id as usize, self.store.node_count())? {
            return Ok(false);
        }
        Ok(true)
    }
}
#[allow(
    clippy::too_many_arguments,
    reason = "Shared search controls and incremental/cancellation callbacks"
)]
pub(super) fn search(
    store: &Store,
    term: &str,
    case: bool,
    regex: bool,
    scope: SearchScope,
    from: u32,
    cancel: &impl Fn() -> bool,
    emit: impl FnMut(Option<u32>, usize, usize) -> Result<bool, String>,
) -> Result<(), String> {
    if term.is_empty() {
        return Ok(());
    }
    if term.len() > 1024 * 1024 {
        return Err("Search text exceeds 1 MiB limit".into());
    }
    store.source.read(0, &mut []).map_err(|e| e.to_string())?;
    let pattern = if regex {
        term.to_owned()
    } else {
        regex::escape(term)
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(!case)
        .build()
        .map_err(|e| format!("Invalid regular expression: {e}"))?;
    let mut walk = Walk {
        store,
        reader: Reader::new(&store.source),
        matcher,
        term,
        regex,
        scope,
        from,
        id: 0,
        cancel,
        emit,
    };
    walk.reader.pos = store.root.range.start;
    if walk.value(Some("$"))? {
        (walk.emit)(None, store.node_count(), store.node_count())?;
    }
    Ok(())
}

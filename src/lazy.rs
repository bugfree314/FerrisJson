//! Sparse, read-only document storage. No temporary index files and no whole DOM.
use super::super::{Document, Kind, Node, NodeIds, SearchScope, Storage, compact, short};
use super::{BLOCK, LoadProgress, Range, Reader, SUBTREE_LIMIT, Snapshot};
#[path = "stream.rs"]
mod stream;
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex, atomic::Ordering},
};

const STRIDE: usize = 256;
const META_BUDGET: usize = 64 * 1024 * 1024;
const PAGE_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Span {
    range: Range,
    kind: Kind,
    nodes: u32,
    children: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Input(std::path::PathBuf);
    impl Input {
        fn new(text: &[u8]) -> Self {
            use std::io::Write;
            let path = std::env::temp_dir().join(format!(
                "FerrisJson-lazy-test-{}-{}.json",
                std::process::id(),
                super::super::SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(text).unwrap();
            Self(path)
        }
        fn open(&self) -> Document {
            Document::open_lazy(&self.0, || false, &LoadProgress::default()).unwrap()
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[test]
    fn sparse_nodes_match_canonical_duplicate_order_and_paths() {
        let text = r#"{"a":{"x":1},"arr":[0,{"n":2},3],"a":{"z":4},"a/b":{"~":5}}"#;
        let input = Input::new(text.as_bytes());
        let lazy = input.open();
        let memory = Document::parse(text).unwrap();
        assert_eq!(lazy.node_count(), memory.node_count());
        assert!(lazy.nodes.is_empty());
        for id in 0..lazy.node_count() {
            assert_eq!(lazy.try_path(id).unwrap(), memory.try_path(id).unwrap());
            assert_eq!(lazy.node(id).unwrap().end(), memory.node(id).unwrap().end());
            assert_eq!(
                lazy.node(id).unwrap().parent(),
                memory.node(id).unwrap().parent()
            );
            assert_eq!(lazy.key(id), memory.key(id));
            assert_eq!(lazy.row_preview(id), memory.row_preview(id));
            assert_eq!(
                lazy.find_path(&if id == 0 {
                    "$".into()
                } else {
                    memory.pointer(id)
                })
                .unwrap(),
                id
            );
        }
        let mut output = Vec::new();
        lazy.write_node(&mut output, 0).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap(),
            *memory.value().unwrap()
        );
        for expression in ["$..*", "$.arr[::-1]", "$.arr[2,0,0]", "$['a/b']['~']"] {
            assert_eq!(
                lazy.query_path(expression).unwrap(),
                memory.query_path(expression).unwrap()
            );
        }
        for term in ["4", "n", "a/b"] {
            assert_eq!(
                lazy.search(term, false, false, SearchScope::All).unwrap(),
                memory.search(term, false, false, SearchScope::All).unwrap()
            );
        }
    }
    #[test]
    fn virtual_rows_cover_large_arrays_and_nested_overrides() {
        let text = serde_json::json!(
            (0..6000)
                .map(|i| serde_json::json!({"n":i,"v":[i,i+1]}))
                .collect::<Vec<_>>()
        )
        .to_string();
        let input = Input::new(text.as_bytes());
        let lazy = input.open();
        let memory = Document::parse(&text).unwrap();
        let mut view = super::super::super::LazyView::new(&lazy).unwrap();
        assert_eq!(view.len(), 6001);
        let mut expanded = vec![false; memory.node_count()];
        expanded[0] = true;
        let id = lazy.find_path("$[5999]").unwrap();
        view.set_open(id, true).unwrap();
        expanded[id] = true;
        let array = lazy.find_path("$[5999].v").unwrap();
        view.set_open(array, true).unwrap();
        expanded[array] = true;
        view.rebuild(&lazy).unwrap();
        let rows = memory.visible(&expanded).unwrap();
        assert_eq!(view.len(), rows.len());
        for row in [0, 1, 257, 4096, rows.len() - 1, rows.len() - 2] {
            assert_eq!(view.row(&lazy, row).unwrap(), rows[row]);
            assert_eq!(view.position(&lazy, rows[row] as usize).unwrap(), Some(row));
        }
        view.all(true);
        view.set_open(id, false).unwrap();
        view.rebuild(&lazy).unwrap();
        expanded.fill(true);
        expanded[id] = false;
        let rows = memory.visible(&expanded).unwrap();
        assert_eq!(view.len(), rows.len());
        assert_eq!(
            view.row(&lazy, view.len() - 1).unwrap(),
            *rows.last().unwrap()
        );
        assert_eq!(view.position(&lazy, array).unwrap(), None);
        let mut matches = super::super::super::MatchSet::default();
        lazy.stream_search(
            ".*",
            false,
            true,
            SearchScope::All,
            0,
            &|| false,
            |id, _, _| {
                if let Some(id) = id {
                    matches.insert(id)?;
                }
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(matches.len(), lazy.node_count());
        assert!(matches.bytes() <= 8192);
        if let Storage::Lazy(store) = &lazy.storage {
            let cache = store.cache.lock().unwrap();
            assert!(cache.bytes <= PAGE_BUDGET);
            assert!(cache.order.len() <= 4096);
            assert!(cache.pages.len() + cache.metas.len() <= 1024);
            assert!(store.source.path.is_none());
        }
    }
    #[test]
    fn strict_validation_cancellation_and_immutable_memory_source() {
        for text in [
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1e+",
            "\"\\uD800\"",
            "[",
            "true false",
        ] {
            let input = Input::new(text.as_bytes());
            assert!(
                Document::open_lazy(&input.0, || false, &LoadProgress::default()).is_err(),
                "{text}"
            );
        }
        let input = Input::new(b"[1,2,3]");
        assert!(
            Document::open_lazy(&input.0, || true, &LoadProgress::default())
                .err()
                .unwrap()
                .contains("cancelled")
        );
        let doc = input.open();
        std::fs::write(&input.0, "[]").unwrap();
        assert_eq!(doc.query_path("$[*]").unwrap().len(), 3);
        assert!(doc.backend_name().contains("memory source"));
    }
    #[test]
    fn filters_and_next_search_preserve_exact_semantics_without_a_dom() {
        let text = r#"{"limit":2,"rows":[{"n":1,"s":"中文","active":true},{"n":2.0,"s":"two","active":false},{"n":123456789012345678901234567890,"s":"three"},{"s":"none","missing":null}]}"#;
        let input = Input::new(text.as_bytes());
        let lazy = input.open();
        let memory = Document::parse(text).unwrap();
        for expression in [
            "$.rows[?(@.n >= $.limit)].s",
            "$.rows[?(@.n == 2)].s",
            "$.rows[?(@.active == true || !@.n)].s",
            "$.rows[?(@.missing)].s",
            "$.rows[?(@.s == '中文')].s",
            "$.rows[?(@.n != 1 && @.n <= 2.0)].s",
        ] {
            assert_eq!(
                lazy.query_path(expression).unwrap(),
                memory.query_path(expression).unwrap(),
                "{expression}"
            );
        }
        assert!(lazy.query_path("$.rows[?(length(@.s) > 1)]").is_err());
        // The Value-backed library rounds this comparison through f64. The
        // on-demand evaluator deliberately keeps the advertised integer precision.
        assert_eq!(
            lazy.query_path("$.rows[?(@.n > 123456789012345678901234567889)].s")
                .unwrap(),
            vec![lazy.find_path("$.rows[2].s").unwrap() as u32]
        );
        let ids = memory.search("s", false, false, SearchScope::Keys).unwrap();
        for origin in [0, ids[0], *ids.last().unwrap()] {
            for count in [1, 2, 7] {
                let after = ids.iter().position(|id| *id > origin).unwrap_or(0);
                let before = ids
                    .iter()
                    .rposition(|id| *id < origin)
                    .unwrap_or(ids.len() - 1);
                assert_eq!(
                    lazy.find_next(
                        "s",
                        false,
                        false,
                        SearchScope::Keys,
                        origin,
                        false,
                        count,
                        &|| false
                    )
                    .unwrap(),
                    Some(ids[(after + count - 1) % ids.len()])
                );
                assert_eq!(
                    lazy.find_next(
                        "s",
                        false,
                        false,
                        SearchScope::Keys,
                        origin,
                        true,
                        count,
                        &|| false
                    )
                    .unwrap(),
                    Some(ids[(before + ids.len() - (count - 1) % ids.len()) % ids.len()])
                );
            }
        }
    }
    #[test]
    fn decoded_stream_search_crosses_blocks_and_retains_long_value_bounds() {
        let text = format!(
            r#"{{"long":"{}\u4e2d\u6587y","Σ":"\u03A3","n":1.2300e+02,"wide":"{}"}}"#,
            "x".repeat(BLOCK - 3),
            "😀".repeat(SUBTREE_LIMIT / 4 + 10)
        );
        let input = Input::new(text.as_bytes());
        let lazy = input.open();
        let memory = Document::parse(&text).unwrap();
        for (term, scope, case) in [
            ("中文y", SearchScope::Values, false),
            ("σ", SearchScope::All, false),
            ("123", SearchScope::All, true),
            ("😀", SearchScope::Values, false),
        ] {
            assert_eq!(
                lazy.search(term, case, false, scope).unwrap(),
                memory.search(term, case, false, scope).unwrap()
            );
        }
        assert!(
            lazy.search(".", false, true, SearchScope::Values)
                .unwrap_err()
                .contains("256 KiB")
        );
        assert!(
            lazy.search("", false, false, SearchScope::All)
                .unwrap()
                .is_empty()
        );
        assert!(
            lazy.preview(lazy.find_path("$.wide").unwrap())
                .contains("truncated")
        );
        assert!(
            lazy.stream_search(
                "x",
                false,
                false,
                SearchScope::All,
                0,
                &|| true,
                |_, _, _| Ok(true)
            )
            .unwrap_err()
            .contains("cancelled")
        );
    }
    #[test]
    fn duplicate_large_branches_retire_unreachable_sparse_metadata() {
        let text = format!(
            "{{\"k\":{},\"k\":0}}",
            serde_json::to_string(&(0..6000).collect::<Vec<_>>()).unwrap()
        );
        let input = Input::new(text.as_bytes());
        let lazy = input.open();
        assert_eq!(lazy.node_count(), 2);
        if let Storage::Lazy(store) = &lazy.storage {
            assert_eq!(store.sparse.len(), 1);
        }
        assert_eq!(
            lazy.search("5999", false, false, SearchScope::Values)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            lazy.search("0", false, false, SearchScope::Values).unwrap(),
            vec![1]
        );
    }
    #[test]
    fn protected_file_source_remains_read_only_and_detects_changed_data() {
        let input = Input::new(b"[1,2,3]");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        let file = options.open(&input.0).unwrap();
        let metadata = file.metadata().unwrap();
        let source = Snapshot {
            file: Mutex::new(Some(file)),
            path: None,
            len: metadata.len(),
            memory: None,
            original: Some((metadata.len(), metadata.modified().ok())),
        };
        let mut bytes = [0; 7];
        source.read(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"[1,2,3]");
        #[cfg(windows)]
        assert!(std::fs::write(&input.0, "[]").is_err());
        #[cfg(not(windows))]
        {
            std::fs::write(&input.0, "[]").unwrap();
            assert!(source.read(0, &mut []).is_err());
        }
        drop(source);
        assert!(input.0.exists());
        std::fs::write(&input.0, "[]").unwrap();
    }
}
#[derive(Clone)]
struct Field {
    name: Arc<str>,
    span: Span,
    prefix: u32,
}
#[derive(Clone, Copy)]
struct Anchor {
    ordinal: u32,
    offset: u64,
    prefix: u32,
}
enum Details {
    Array(Vec<Anchor>),
    Object(Vec<Field>),
    Scalar,
}
struct Meta {
    span: Span,
    details: Details,
    duplicate: bool,
}
impl Meta {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.details {
                Details::Array(a) => a.capacity() * std::mem::size_of::<Anchor>(),
                Details::Object(f) => {
                    f.capacity() * std::mem::size_of::<Field>()
                        + f.iter().map(|f| f.name.len() + 32).sum::<usize>()
                }
                Details::Scalar => 0,
            }
    }
}
struct Scanner<'a, F> {
    reader: Reader<'a>,
    cancel: F,
    progress: Option<&'a LoadProgress>,
    known: Option<&'a HashMap<u64, Arc<Meta>>>,
    meta: HashMap<u64, Arc<Meta>>,
    allocated: usize,
    active_anchors: usize,
    keys_work: usize,
    force: u64,
}
impl<'a, F: Fn() -> bool> Scanner<'a, F> {
    fn new(
        source: &'a Snapshot,
        cancel: F,
        known: Option<&'a HashMap<u64, Arc<Meta>>>,
        force: u64,
    ) -> Self {
        Self {
            reader: Reader::new(source),
            cancel,
            progress: None,
            known,
            meta: HashMap::new(),
            allocated: 0,
            active_anchors: 0,
            keys_work: 0,
            force,
        }
    }
    fn check(&self) -> Result<(), String> {
        if (self.cancel)() {
            Err("Loading cancelled".into())
        } else {
            Ok(())
        }
    }
    fn tick(&self) -> Result<(), String> {
        self.check()?;
        if let Some(progress) = self.progress {
            progress.done.store(self.reader.pos, Ordering::Relaxed);
        }
        Ok(())
    }
    fn space(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b' ' | b'\r' | b'\n' | b'\t')) {
            self.reader.next()?;
            if self.reader.pos & 4095 == 0 {
                self.tick()?;
            }
        }
        Ok(())
    }
    fn string(&mut self, key: bool) -> Result<String, String> {
        self.reader.expect(b'"')?;
        let mut result = String::new();
        while let Some(c) = self.reader.character()? {
            if key {
                if result.len() > 1024 * 1024 - c.len_utf8() {
                    return Err("Object key exceeds 1 MiB limit".into());
                }
                result.push(c);
            }
            if self.reader.pos & 4095 < 4 {
                self.tick()?;
            }
        }
        Ok(result)
    }
    fn number(&mut self) -> Result<(), String> {
        if self.reader.peek()? == Some(b'-') {
            self.reader.next()?;
        }
        match self.reader.peek()? {
            Some(b'0') => {
                self.reader.next()?;
            }
            Some(b'1'..=b'9') => self.digits()?,
            _ => return Err(self.reader.error("invalid number")),
        }
        if self.reader.peek()? == Some(b'.') {
            self.reader.next()?;
            self.required_digits()?;
        }
        if matches!(self.reader.peek()?, Some(b'e' | b'E')) {
            self.reader.next()?;
            if matches!(self.reader.peek()?, Some(b'+' | b'-')) {
                self.reader.next()?;
            }
            self.required_digits()?;
        }
        Ok(())
    }
    fn digits(&mut self) -> Result<(), String> {
        while matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
            self.reader.next()?;
            if self.reader.pos & 4095 == 0 {
                self.tick()?;
            }
        }
        Ok(())
    }
    fn required_digits(&mut self) -> Result<(), String> {
        if !matches!(self.reader.peek()?, Some(b'0'..=b'9')) {
            return Err(self.reader.error("invalid number"));
        }
        self.digits()
    }
    fn value(&mut self, depth: usize) -> Result<Span, String> {
        self.check()?;
        self.space()?;
        let start = self.reader.pos;
        if start != self.force
            && let Some(meta) = self.known.and_then(|m| m.get(&start))
        {
            self.reader.pos = meta.span.range.end;
            return Ok(meta.span);
        }
        let mut nodes: usize = 1;
        let mut children = 0;
        let mut duplicate = false;
        let (kind, details) = match self.reader.peek()? {
            Some(b'"') => {
                self.string(false)?;
                (Kind::String, Details::Scalar)
            }
            Some(b'-' | b'0'..=b'9') => {
                self.number()?;
                (Kind::Number, Details::Scalar)
            }
            Some(b't') => {
                for b in b"true" {
                    self.reader.expect(*b)?;
                }
                (Kind::Boolean, Details::Scalar)
            }
            Some(b'f') => {
                for b in b"false" {
                    self.reader.expect(*b)?;
                }
                (Kind::Boolean, Details::Scalar)
            }
            Some(b'n') => {
                for b in b"null" {
                    self.reader.expect(*b)?;
                }
                (Kind::Null, Details::Scalar)
            }
            Some(b'[') => {
                if depth >= 127 {
                    return Err(self.reader.error("recursion limit exceeded"));
                }
                self.reader.next()?;
                self.space()?;
                let mut anchors = Vec::new();
                if self.reader.peek()? != Some(b']') {
                    loop {
                        if children % STRIDE == 0 {
                            if anchors.len() == anchors.capacity() {
                                let extra = anchors.capacity().clamp(4, 4096);
                                let bytes = extra * std::mem::size_of::<Anchor>();
                                if self.active_anchors + self.allocated + bytes > META_BUDGET {
                                    return Err("Sparse checkpoints exceed 64 MiB budget".into());
                                }
                                anchors
                                    .try_reserve_exact(extra)
                                    .map_err(|e| format!("Cannot allocate checkpoints: {e}"))?;
                                self.active_anchors += bytes;
                            }
                            anchors.push(Anchor {
                                ordinal: compact(children)?,
                                offset: self.reader.pos,
                                prefix: compact(nodes)?,
                            });
                        }
                        let child = self.value(depth + 1)?;
                        nodes = nodes
                            .checked_add(child.nodes as usize)
                            .ok_or("Node count overflow")?;
                        compact(nodes)?;
                        children += 1;
                        self.space()?;
                        if self.reader.peek()? != Some(b',') {
                            break;
                        }
                        self.reader.next()?;
                        self.space()?;
                    }
                }
                self.reader.expect(b']')?;
                self.active_anchors -= anchors.capacity() * std::mem::size_of::<Anchor>();
                (Kind::Array, Details::Array(anchors))
            }
            Some(b'{') => {
                if depth >= 127 {
                    return Err(self.reader.error("recursion limit exceeded"));
                }
                self.reader.next()?;
                self.space()?;
                let mut fields: Vec<Field> = Vec::new();
                let mut names: HashMap<Arc<str>, usize> = HashMap::new();
                let mut cost = 0;
                if self.reader.peek()? != Some(b'}') {
                    loop {
                        let name: Arc<str> = Arc::from(self.string(true)?);
                        self.space()?;
                        self.reader.expect(b':')?;
                        if let Some(&index) = names.get(&name) {
                            duplicate = true;
                            let old = fields[index].span.range;
                            self.meta.retain(|start, meta| {
                                if *start >= old.start && *start < old.end {
                                    self.allocated -= meta.bytes() + 64;
                                    false
                                } else {
                                    true
                                }
                            });
                        }
                        let span = self.value(depth + 1)?;
                        if let Some(&index) = names.get(&name) {
                            fields[index].span = span;
                        } else {
                            let extra = name.len() + std::mem::size_of::<Field>() * 3 + 64;
                            if self.keys_work + extra > META_BUDGET {
                                return Err("Active object keys exceed 64 MiB budget".into());
                            }
                            self.keys_work += extra;
                            cost += extra;
                            names.insert(Arc::clone(&name), fields.len());
                            fields.push(Field {
                                name,
                                span,
                                prefix: 0,
                            });
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
                self.keys_work -= cost;
                children = fields.len();
                for field in &mut fields {
                    field.prefix = compact(nodes)?;
                    nodes += field.span.nodes as usize;
                    compact(nodes)?;
                }
                (Kind::Object, Details::Object(fields))
            }
            _ => return Err(self.reader.error("unexpected token")),
        };
        let span = Span {
            range: Range {
                start,
                end: self.reader.pos,
            },
            kind,
            nodes: compact(nodes)?,
            children: compact(children)?,
        };
        if start == self.force
            || duplicate
            || nodes >= 1024
            || span.range.end - start >= BLOCK as u64
        {
            let meta = Arc::new(Meta {
                span,
                details,
                duplicate,
            });
            let bytes = meta.bytes() + 64;
            if self.allocated + self.active_anchors + bytes > META_BUDGET {
                return Err("Sparse metadata exceeds 64 MiB budget".into());
            }
            self.allocated += bytes;
            self.meta.insert(start, meta);
        }
        self.tick()?;
        Ok(span)
    }
}

#[derive(Clone)]
struct Child {
    span: Span,
    prefix: u32,
    ordinal: u32,
    name: Option<Arc<str>>,
}
struct Page {
    children: Vec<Child>,
}
impl Page {
    fn bytes(&self) -> usize {
        self.children.capacity() * std::mem::size_of::<Child>()
            + self
                .children
                .iter()
                .filter_map(|c| c.name.as_ref())
                .map(|n| n.len() + 32)
                .sum::<usize>()
            + 64
    }
}
#[derive(Clone, Copy)]
enum CacheKey {
    Page((u64, usize)),
    Meta(u64),
}
#[derive(Default)]
struct Cache {
    order: std::collections::VecDeque<(CacheKey, u64)>,
    pages: HashMap<(u64, usize), (Arc<Page>, u64)>,
    metas: HashMap<u64, (Arc<Meta>, u64)>,
    bytes: usize,
    clock: u64,
}
impl Cache {
    fn note(&mut self, key: CacheKey, stamp: u64) {
        self.order.push_back((key, stamp));
        if self.order.len() > 4096 {
            let pages = &self.pages;
            let metas = &self.metas;
            self.order.retain(|(key, stamp)| match key {
                CacheKey::Page(key) => pages.get(key).is_some_and(|(_, current)| current == stamp),
                CacheKey::Meta(key) => metas.get(key).is_some_and(|(_, current)| current == stamp),
            });
        }
    }
    fn make_room(&mut self, cost: usize) {
        while self.bytes + cost > PAGE_BUDGET || self.pages.len() + self.metas.len() >= 1024 {
            let Some((key, stamp)) = self.order.pop_front() else {
                break;
            };
            match key {
                CacheKey::Page(key)
                    if self
                        .pages
                        .get(&key)
                        .is_some_and(|(_, current)| *current == stamp) =>
                {
                    self.bytes -= self.pages.remove(&key).unwrap().0.bytes();
                }
                CacheKey::Meta(key)
                    if self
                        .metas
                        .get(&key)
                        .is_some_and(|(_, current)| *current == stamp) =>
                {
                    self.bytes -= self.metas.remove(&key).unwrap().0.bytes() + 64;
                }
                _ => {}
            }
        }
    }
}
pub(in super::super) struct Store {
    source: Snapshot,
    root: Span,
    sparse: HashMap<u64, Arc<Meta>>,
    cache: Mutex<Cache>,
    plans: Mutex<PlanCache>,
}
#[derive(Default)]
struct PlanCache {
    entries: HashMap<String, (Arc<super::QueryPlan>, usize, u64)>,
    bytes: usize,
    clock: u64,
}
struct Located {
    node: Node,
    span: Span,
    name: Option<Arc<str>>,
}

pub(in super::super) fn open(
    path: &Path,
    cancelled: impl Fn() -> bool,
    progress: &LoadProgress,
) -> Result<Document, String> {
    let source = Snapshot::open_lazy(path, &cancelled, progress)?;
    progress.phase.store(1, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let mut scanner = Scanner::new(&source, &cancelled, None, u64::MAX);
    scanner.progress = Some(progress);
    while scanner.reader.peek()? == Some(0xef) {
        scanner.reader.expect(0xef)?;
        scanner.reader.expect(0xbb)?;
        scanner.reader.expect(0xbf)?;
    }
    scanner.space()?;
    scanner.force = scanner.reader.pos;
    let root = scanner.value(0)?;
    scanner.space()?;
    if scanner.reader.peek()?.is_some() {
        return Err(scanner.reader.error("trailing characters"));
    }
    scanner.check()?;
    let sparse = scanner.meta;
    progress.phase.store(2, Ordering::Relaxed);
    Ok(Document {
        bytes: usize::try_from(source.len).map_err(|_| "File size exceeds address range")?,
        storage: Storage::Lazy(Box::new(Store {
            source,
            root,
            sparse,
            cache: Mutex::new(Cache::default()),
            plans: Mutex::new(PlanCache::default()),
        })),
        nodes: Vec::new(),
        children: Vec::new(),
        keys: Vec::new(),
        key_ids: HashMap::new(),
    })
}
impl Store {
    pub(in super::super) fn query(
        &self,
        doc: &Document,
        expression: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<NodeIds, String> {
        if expression.len() > 16 * 1024 {
            return Err("Query exceeds 16 KiB planning budget".into());
        }
        let cached = {
            let mut cache = self.plans.lock().map_err(|_| "Query cache lock failed")?;
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            cache.entries.get_mut(expression).map(|(plan, _, stamp)| {
                *stamp = clock;
                Arc::clone(plan)
            })
        };
        let plan = if let Some(plan) = cached {
            plan
        } else {
            serde_json_path::JsonPath::parse(expression)
                .map_err(|e| format!("Invalid JSONPath: {e}"))?;
            let plan = Arc::new(super::parse_plan(expression)?);
            let cost = expression.len() + super::plan_bytes(&plan) + 128;
            if cost <= 256 * 1024 {
                let mut cache = self.plans.lock().map_err(|_| "Query cache lock failed")?;
                if !cache.entries.contains_key(expression) {
                    while cache.bytes + cost > 256 * 1024 || cache.entries.len() >= 16 {
                        let key = cache
                            .entries
                            .iter()
                            .min_by_key(|(_, (_, _, stamp))| stamp)
                            .unwrap()
                            .0
                            .clone();
                        cache.bytes -= cache.entries.remove(&key).unwrap().1;
                    }
                    cache.clock = cache.clock.wrapping_add(1);
                    let clock = cache.clock;
                    cache.bytes += cost;
                    cache
                        .entries
                        .insert(expression.into(), (Arc::clone(&plan), cost, clock));
                }
            }
            plan
        };
        super::execute_plan(doc, &plan, cancelled)
    }
    pub(in super::super) fn child_ordinal(
        &self,
        parent: usize,
        ordinal: usize,
    ) -> Result<usize, String> {
        let found = self.locate(parent)?;
        Ok(parent + self.child(found.span, ordinal)?.prefix as usize)
    }
    pub(in super::super) fn index_bytes(&self) -> usize {
        self.sparse.values().map(|m| m.bytes() + 64).sum::<usize>()
            + self.cache.lock().unwrap_or_else(|e| e.into_inner()).bytes
            + self.plans.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
    pub(in super::super) fn backend_name(&self) -> &'static str {
        if self.source.memory.is_some() {
            "On-demand / memory source"
        } else {
            "On-demand / read-only file"
        }
    }
    pub(in super::super) fn subtree(&self, id: usize) -> Result<Arc<Value>, String> {
        let span = self.locate(id)?.span;
        if span.range.end - span.range.start > SUBTREE_LIMIT as u64 || span.nodes > 16_384 {
            return Err("Subtree exceeds on-demand parsing budget".into());
        }
        let mut bytes = vec![0; (span.range.end - span.range.start) as usize];
        self.source
            .read(span.range.start, &mut bytes)
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes)
            .map(Arc::new)
            .map_err(|e| e.to_string())
    }
    pub(in super::super) fn search(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
        cancelled: impl Fn() -> bool,
    ) -> Result<NodeIds, String> {
        let mut result = Vec::new();
        self.scan(term, case, regex, scope, 0, &cancelled, |id, _, _| {
            if let Some(id) = id {
                super::result_id(&mut result, id as usize)?;
            }
            Ok(true)
        })?;
        Ok(result)
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Shared search controls with incremental and cancellation callbacks"
    )]
    pub(in super::super) fn scan(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
        from: u32,
        cancelled: &impl Fn() -> bool,
        emit: impl FnMut(Option<u32>, usize, usize) -> Result<bool, String>,
    ) -> Result<(), String> {
        stream::search(self, term, case, regex, scope, from, cancelled, emit)
    }
    pub(in super::super) fn node_count(&self) -> usize {
        self.root.nodes as usize
    }
    pub(in super::super) fn memory_bytes(&self) -> usize {
        self.source.memory.as_ref().map_or(0, |b| b.len()) + self.index_bytes()
    }
    fn describe(&self, span: Span) -> Result<Arc<Meta>, String> {
        if let Some(meta) = self.sparse.get(&span.range.start) {
            return Ok(Arc::clone(meta));
        }
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "Container cache lock failed")?;
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            if let Some((meta, stamp)) = cache.metas.get_mut(&span.range.start) {
                *stamp = clock;
                let found = Arc::clone(meta);
                cache.note(CacheKey::Meta(span.range.start), clock);
                return Ok(found);
            }
        }
        let mut scanner =
            Scanner::new(&self.source, || false, Some(&self.sparse), span.range.start);
        scanner.reader.pos = span.range.start;
        scanner.value(0)?;
        let meta = scanner
            .meta
            .remove(&span.range.start)
            .ok_or("Missing container metadata")?;
        self.remember(meta)
    }
    fn remember(&self, meta: Arc<Meta>) -> Result<Arc<Meta>, String> {
        let start = meta.span.range.start;
        let cost = meta.bytes() + 64;
        if cost <= PAGE_BUDGET {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "Container cache lock failed")?;
            if let Some((meta, _)) = cache.metas.get(&start) {
                return Ok(Arc::clone(meta));
            }
            cache.make_room(cost);
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            cache.bytes += cost;
            cache.metas.insert(start, (Arc::clone(&meta), clock));
            cache.note(CacheKey::Meta(start), clock);
        }
        Ok(meta)
    }
    fn page(&self, span: Span, page: usize) -> Result<Arc<Page>, String> {
        let key = (span.range.start, page);
        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "Container cache lock failed")?;
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            if let Some((page, stamp)) = cache.pages.get_mut(&key) {
                *stamp = clock;
                let found = Arc::clone(page);
                cache.note(CacheKey::Page(key), clock);
                return Ok(found);
            }
        }
        let meta = self.describe(span)?;
        let start = page * STRIDE;
        if start >= span.children as usize {
            return Err("Child page outside container".into());
        }
        let mut children = Vec::new();
        match &meta.details {
            Details::Object(fields) => {
                for (ordinal, field) in fields.iter().enumerate().skip(start).take(STRIDE) {
                    children.push(Child {
                        span: field.span,
                        prefix: field.prefix,
                        ordinal: ordinal as u32,
                        name: Some(Arc::clone(&field.name)),
                    });
                }
            }
            Details::Array(anchors) => {
                let anchor = anchors.get(page).ok_or("Missing array checkpoint")?;
                if anchor.ordinal as usize != start {
                    return Err("Invalid array checkpoint ordinal".into());
                }
                let mut scanner =
                    Scanner::new(&self.source, || false, Some(&self.sparse), u64::MAX);
                scanner.reader.pos = anchor.offset;
                let mut prefix = anchor.prefix;
                for ordinal in start..(start + STRIDE).min(span.children as usize) {
                    scanner.force = if self.sparse.contains_key(&scanner.reader.pos) {
                        u64::MAX
                    } else {
                        scanner.reader.pos
                    };
                    let child = scanner.value(0)?;
                    if let Some(meta) = scanner.meta.remove(&child.range.start) {
                        scanner.allocated -= meta.bytes() + 64;
                        if matches!(child.kind, Kind::Object | Kind::Array) {
                            self.remember(meta)?;
                        }
                    }
                    children.push(Child {
                        span: child,
                        prefix,
                        ordinal: ordinal as u32,
                        name: None,
                    });
                    prefix += child.nodes;
                    scanner.space()?;
                    if ordinal + 1 < span.children as usize {
                        scanner.reader.expect(b',')?;
                        scanner.space()?;
                    }
                }
            }
            _ => return Err("Scalar has no child pages".into()),
        }
        let page = Arc::new(Page { children });
        let cost = page.bytes();
        if cost <= PAGE_BUDGET {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "Container cache lock failed")?;
            if let Some((existing, _)) = cache.pages.get(&key) {
                return Ok(Arc::clone(existing));
            }
            cache.make_room(cost);
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            cache.bytes += cost;
            cache.pages.insert(key, (Arc::clone(&page), clock));
            cache.note(CacheKey::Page(key), clock);
        }
        Ok(page)
    }
    fn child(&self, span: Span, ordinal: usize) -> Result<Child, String> {
        self.page(span, ordinal / STRIDE)?
            .children
            .get(ordinal % STRIDE)
            .cloned()
            .ok_or_else(|| "Invalid child ordinal".into())
    }
    fn locate(&self, target: usize) -> Result<Located, String> {
        if target >= self.node_count() {
            return Err("Invalid node ID".into());
        }
        let mut span = self.root;
        let mut id = 0;
        let mut parent = u32::MAX;
        let mut depth = 0;
        let mut segment = 0;
        let mut name = None;
        loop {
            if target == id {
                return Ok(Located {
                    node: Node {
                        parent,
                        depth,
                        end: compact(id + span.nodes as usize)?,
                        segment,
                        child_start: 0,
                        child_count: span.children,
                        kind: span.kind,
                    },
                    span,
                    name,
                });
            }
            let relative = target - id;
            let meta = self.describe(span)?;
            let page = match &meta.details {
                Details::Array(a) => a
                    .partition_point(|a| a.prefix as usize <= relative)
                    .saturating_sub(1),
                Details::Object(f) => {
                    f.partition_point(|f| f.prefix as usize <= relative)
                        .saturating_sub(1)
                        / STRIDE
                }
                _ => return Err("Node outside container".into()),
            };
            let page = self.page(span, page)?;
            let child = page
                .children
                .iter()
                .rev()
                .find(|c| c.prefix as usize <= relative)
                .ok_or("Missing child location")?;
            parent = compact(id)?;
            id += child.prefix as usize;
            depth += 1;
            segment = child.ordinal;
            name = child.name.clone();
            span = child.span;
        }
    }
    pub(in super::super) fn node(&self, id: usize) -> Result<Node, String> {
        Ok(self.locate(id)?.node)
    }
    pub(in super::super) fn key(&self, id: usize) -> Result<String, String> {
        let found = self.locate(id)?;
        Ok(found.name.map_or_else(
            || {
                if id == 0 {
                    "$".into()
                } else {
                    found.node.segment.to_string()
                }
            },
            |n| n.to_string(),
        ))
    }
    pub(in super::super) fn child_index(
        &self,
        id: usize,
        index: usize,
    ) -> Result<Option<usize>, String> {
        let located = self.locate(id)?;
        if !matches!(located.span.kind, Kind::Array) || index >= located.span.children as usize {
            return Ok(None);
        }
        Ok(Some(id + self.child(located.span, index)?.prefix as usize))
    }
    pub(in super::super) fn child_name(
        &self,
        id: usize,
        name: &str,
    ) -> Result<Option<usize>, String> {
        let located = self.locate(id)?;
        if !matches!(located.span.kind, Kind::Object) {
            return Ok(None);
        }
        let meta = self.describe(located.span)?;
        if let Details::Object(fields) = &meta.details {
            return Ok(fields
                .iter()
                .find(|f| f.name.as_ref() == name)
                .map(|f| id + f.prefix as usize));
        }
        Ok(None)
    }
    pub(in super::super) fn row_preview(&self, id: usize) -> Result<String, String> {
        let found = self.locate(id)?;
        match found.span.kind {
            Kind::Object => Ok(format!("{{ {} keys }}", found.span.children)),
            Kind::Array => Ok(format!("[ {} items ]", found.span.children)),
            Kind::String => {
                let mut reader = Reader::new(&self.source);
                reader.pos = found.span.range.start;
                reader.expect(b'"')?;
                let mut text = String::new();
                for _ in 0..160 {
                    match reader.character()? {
                        Some(c) => text.push(c),
                        None => return serde_json::to_string(&text).map_err(|e| e.to_string()),
                    }
                }
                if reader.character()?.is_some() {
                    text.push('…');
                }
                serde_json::to_string(&text).map_err(|e| e.to_string())
            }
            Kind::Number
                if found.span.range.end - found.span.range.start > SUBTREE_LIMIT as u64 =>
            {
                let mut bytes = [0; 160];
                self.source
                    .read(found.span.range.start, &mut bytes)
                    .map_err(|e| e.to_string())?;
                Ok(format!(
                    "{}…",
                    std::str::from_utf8(&bytes).map_err(|e| e.to_string())?
                ))
            }
            _ => {
                let mut reader = Reader::new(&self.source);
                Ok(short(&reader.scalar(found.span.range)?.to_string(), 160))
            }
        }
    }
    pub(in super::super) fn write(
        &self,
        doc: &Document,
        output: &mut dyn Write,
        ids: &[u32],
        array: bool,
    ) -> io::Result<()> {
        fn write(
            store: &Store,
            reader: &mut Reader<'_>,
            output: &mut dyn Write,
            span: Span,
            depth: usize,
        ) -> io::Result<()> {
            let container = matches!(span.kind, Kind::Array | Kind::Object);
            if container {
                let object = matches!(span.kind, Kind::Object);
                output.write_all(if object { b"{" } else { b"[" })?;
                for ordinal in 0..span.children as usize {
                    let child = store.child(span, ordinal).map_err(io::Error::other)?;
                    if ordinal > 0 {
                        output.write_all(b",")?;
                    }
                    output.write_all(b"\n")?;
                    output.write_all(&[b' '; 512][..(depth + 1) * 2])?;
                    if object {
                        serde_json::to_writer(&mut *output, child.name.as_ref().unwrap().as_ref())
                            .map_err(io::Error::other)?;
                        output.write_all(b": ")?;
                    }
                    write(store, reader, output, child.span, depth + 1)?;
                }
                if span.children > 0 {
                    output.write_all(b"\n")?;
                    output.write_all(&[b' '; 512][..depth * 2])?;
                }
                output.write_all(if object { b"}" } else { b"]" })
            } else {
                reader.pos = span.range.start;
                while reader.pos < span.range.end {
                    reader.peek().map_err(io::Error::other)?;
                    let count = ((reader.start + reader.count as u64).min(span.range.end)
                        - reader.pos) as usize;
                    let start = (reader.pos - reader.start) as usize;
                    output.write_all(&reader.buffer[start..start + count])?;
                    reader.pos += count as u64;
                }
                Ok(())
            }
        }
        let _ = doc;
        let mut reader = Reader::new(&self.source);
        if array {
            output.write_all(b"[")?;
        }
        for (ordinal, id) in ids.iter().enumerate() {
            if ordinal > 0 {
                output.write_all(b",\n")?;
            }
            write(
                self,
                &mut reader,
                output,
                self.locate(*id as usize).map_err(io::Error::other)?.span,
                0,
            )?;
        }
        if array {
            output.write_all(b"]")?;
        }
        Ok(())
    }
}

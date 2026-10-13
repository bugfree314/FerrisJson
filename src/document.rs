use regex::RegexBuilder;
use serde_json::Value;
use serde_json_path::{JsonPath, PathElement};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Write},
    sync::Arc,
};

#[derive(Clone, Copy)]
enum Kind {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}
impl Kind {
    fn of(value: &Value) -> Self {
        match value {
            Value::Object(_) => Self::Object,
            Value::Array(_) => Self::Array,
            Value::String(_) => Self::String,
            Value::Number(_) => Self::Number,
            Value::Bool(_) => Self::Boolean,
            Value::Null => Self::Null,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }
}

// Preorder node IDs remain stable. Strings and complete paths are never stored per node.
pub struct Node {
    parent: u32,
    pub depth: u32,
    end: u32,
    segment: u32,
    child_start: u32,
    child_count: u32,
    kind: Kind,
}
impl Node {
    pub fn parent(&self) -> Option<usize> {
        (self.parent != u32::MAX).then_some(self.parent as usize)
    }
    pub fn end(&self) -> usize {
        self.end as usize
    }
    pub fn kind(&self) -> &'static str {
        self.kind.name()
    }
    pub fn container(&self) -> bool {
        matches!(self.kind, Kind::Object | Kind::Array)
    }
}
pub struct Document {
    pub value: Value,
    pub nodes: Vec<Node>,
    pub bytes: usize,
    children: Vec<u32>,
    keys: Vec<Arc<str>>,
    key_ids: HashMap<Arc<str>, u32>,
}
struct IndexBuilder {
    nodes: Vec<Node>,
    children: Vec<u32>,
    keys: Vec<Arc<str>>,
    key_ids: HashMap<Arc<str>, u32>,
}
impl IndexBuilder {
    fn index(
        &mut self,
        value: &Value,
        segment: u32,
        parent: u32,
        depth: u32,
    ) -> Result<u32, String> {
        let id = self.nodes.len();
        let count = match value {
            Value::Object(v) => v.len(),
            Value::Array(v) => v.len(),
            _ => 0,
        };
        let start = self.children.len();
        self.children.resize(start + count, 0);
        self.nodes.push(Node {
            parent,
            depth,
            end: compact(id + 1)?,
            segment,
            child_start: compact(start)?,
            child_count: compact(count)?,
            kind: Kind::of(value),
        });
        match value {
            Value::Object(map) => {
                for (ordinal, (key, child)) in map.iter().enumerate() {
                    let segment = if let Some(&index) = self.key_ids.get(key.as_str()) {
                        index
                    } else {
                        let index = compact(self.keys.len())?;
                        let key: Arc<str> = Arc::from(key.as_str());
                        self.key_ids.insert(Arc::clone(&key), index);
                        self.keys.push(key);
                        index
                    };
                    let child_id = self.index(child, segment, compact(id)?, depth + 1)?;
                    self.children[start + ordinal] = child_id;
                }
                // Lookup order is independent of display order, which stays in preorder.
                self.children[start..start + count]
                    .sort_unstable_by_key(|&child| self.nodes[child as usize].segment);
            }
            Value::Array(array) => {
                for (ordinal, child) in array.iter().enumerate() {
                    let child_id = self.index(child, compact(ordinal)?, compact(id)?, depth + 1)?;
                    self.children[start + ordinal] = child_id;
                }
            }
            _ => (),
        }
        self.nodes[id].end = compact(self.nodes.len())?;
        compact(id)
    }
}
fn compact(value: usize) -> Result<u32, String> {
    u32::try_from(value)
        .map_err(|_| "Document exceeds the 32-bit index limit (4,294,967,295 nodes)".into())
}
fn node_count(value: &Value) -> Result<usize, String> {
    let add = |count: usize, child: &Value| {
        let total = count
            .checked_add(node_count(child)?)
            .ok_or("Node count overflow")?;
        compact(total)?;
        Ok(total)
    };
    match value {
        Value::Object(v) => v.values().try_fold(1, add),
        Value::Array(v) => v.iter().try_fold(1, add),
        _ => Ok(1),
    }
}
impl Document {
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_owned(text.to_owned())
    }
    /// Consume input so its allocation is released before the full index is built.
    pub fn parse_owned(text: String) -> Result<Self, String> {
        let bytes = text.len();
        let value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        drop(text);
        Self::from_value(value, bytes)
    }
    pub fn from_value(value: Value, bytes: usize) -> Result<Self, String> {
        // Counting avoids geometric capacity growth for the two large index arrays.
        let count = node_count(&value)?;
        let mut index = IndexBuilder {
            nodes: Vec::with_capacity(count),
            children: Vec::with_capacity(count.saturating_sub(1)),
            keys: Vec::new(),
            key_ids: HashMap::new(),
        };
        index.index(&value, 0, u32::MAX, 0)?;
        Ok(Self {
            value,
            nodes: index.nodes,
            bytes,
            children: index.children,
            keys: index.keys,
            key_ids: index.key_ids,
        })
    }
    #[allow(dead_code, reason = "Used by the standalone memory benchmark")]
    pub fn structure_capacity_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.children.capacity() * std::mem::size_of::<u32>()
    }
    pub fn at(&self, id: usize) -> &Value {
        let node = &self.nodes[id];
        let Some(parent) = node.parent() else {
            return &self.value;
        };
        match self.at(parent) {
            Value::Object(map) => &map[self.keys[node.segment as usize].as_ref()],
            Value::Array(array) => &array[node.segment as usize],
            _ => unreachable!("only containers have children"),
        }
    }
    pub fn key(&self, id: usize) -> Cow<'_, str> {
        let node = &self.nodes[id];
        match node.parent() {
            None => Cow::Borrowed("$"),
            Some(parent) if matches!(self.nodes[parent].kind, Kind::Array) => {
                Cow::Owned(node.segment.to_string())
            }
            Some(_) => Cow::Borrowed(&self.keys[node.segment as usize]),
        }
    }
    fn ancestors(&self, mut id: usize) -> Vec<usize> {
        let mut result = Vec::with_capacity(self.nodes[id].depth as usize);
        while let Some(parent) = self.nodes[id].parent() {
            result.push(id);
            id = parent;
        }
        result.reverse();
        result
    }
    pub fn path(&self, id: usize) -> String {
        let mut path = String::from("$");
        for child in self.ancestors(id) {
            let node = &self.nodes[child];
            if matches!(self.nodes[node.parent().unwrap()].kind, Kind::Array) {
                path.push_str(&format!("[{}]", node.segment));
            } else {
                let key = &self.keys[node.segment as usize];
                if is_identifier(key) {
                    path.push('.');
                    path.push_str(key);
                } else {
                    path.push('[');
                    path.push_str(&serde_json::to_string(key.as_ref()).unwrap());
                    path.push(']');
                }
            }
        }
        path
    }
    #[cfg(test)]
    fn pointer(&self, id: usize) -> String {
        let mut pointer = String::new();
        for child in self.ancestors(id) {
            pointer.push('/');
            pointer.push_str(&self.key(child).replace('~', "~0").replace('/', "~1"));
        }
        pointer
    }
    fn child_name(&self, parent: usize, name: &str) -> Option<usize> {
        if !matches!(self.nodes[parent].kind, Kind::Object) {
            return None;
        }
        let &segment = self.key_ids.get(name)?;
        let node = &self.nodes[parent];
        let children = &self.children
            [node.child_start as usize..(node.child_start as usize + node.child_count as usize)];
        children
            .binary_search_by_key(&segment, |&id| self.nodes[id as usize].segment)
            .ok()
            .map(|i| children[i] as usize)
    }
    fn child_index(&self, parent: usize, index: usize) -> Option<usize> {
        let node = &self.nodes[parent];
        (matches!(node.kind, Kind::Array) && index < node.child_count as usize)
            .then(|| self.children[node.child_start as usize + index] as usize)
    }
    fn pointer_id(&self, pointer: &str) -> Option<usize> {
        let mut id = 0;
        for token in pointer.strip_prefix('/')?.split('/') {
            let mut bytes = token.bytes();
            while let Some(byte) = bytes.next() {
                if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
                    return None;
                }
            }
            let token = token.replace("~1", "/").replace("~0", "~");
            id = if matches!(self.nodes[id].kind, Kind::Array) {
                if token.starts_with('+') || (token.len() > 1 && token.starts_with('0')) {
                    return None;
                }
                self.child_index(id, token.parse().ok()?)?
            } else {
                self.child_name(id, &token)?
            };
        }
        Some(id)
    }
    pub fn find_path(&self, path: &str) -> Result<usize, String> {
        let ids = self.query_path(path)?;
        match ids.as_slice() {
            [id] => Ok(*id),
            [] => Err(format!("Path not found: {path}")),
            _ => Err("Path selected multiple nodes; use --query to retrieve all results".into()),
        }
    }
    pub fn query_path(&self, expression: &str) -> Result<Vec<usize>, String> {
        self.query_path_cancellable(expression, || false)
    }
    pub fn query_path_cancellable(
        &self,
        expression: &str,
        cancelled: impl Fn() -> bool,
    ) -> Result<Vec<usize>, String> {
        if cancelled() {
            return Err("Query cancelled".into());
        }
        let expression = expression.trim();
        if expression.starts_with('/') {
            return Ok(self.pointer_id(expression).into_iter().collect());
        }
        let normalized = if expression == "." {
            "$".into()
        } else if expression.starts_with('.') || expression.starts_with('[') {
            format!("${expression}")
        } else {
            expression.to_owned()
        };
        let query = JsonPath::parse(&normalized).map_err(|e| format!("Invalid JSONPath: {e}"))?;
        let results = query.query_located(&self.value);
        if cancelled() {
            return Err("Query cancelled".into());
        }
        results
            .iter()
            .enumerate()
            .map(|(index, node)| {
                if index % 1024 == 0 && cancelled() {
                    return Err("Query cancelled".into());
                }
                let mut id = 0;
                for element in node.location().iter() {
                    id = match element {
                        PathElement::Name(name) => self.child_name(id, name),
                        PathElement::Index(index) => self.child_index(id, *index),
                    }
                    .ok_or_else(|| "Query result not found in tree".to_string())?;
                }
                Ok(id)
            })
            .collect()
    }
    pub fn row_preview(&self, id: usize) -> String {
        match self.at(id) {
            Value::Object(v) => format!("{{ {} keys }}", v.len()),
            Value::Array(v) => format!("[ {} items ]", v.len()),
            Value::String(v) => serde_json::to_string(&short(v, 160)).unwrap(),
            Value::Number(v) => short(v.as_str(), 160),
            value => value.to_string(),
        }
    }
    pub fn preview(&self, id: usize) -> String {
        bounded_preview(self.at(id))
    }
    pub fn results_json(&self, ids: &[usize]) -> String {
        serde_json::to_string_pretty(&SelectedValues { doc: self, ids }).unwrap()
    }
    pub fn results_preview(&self, ids: &[usize]) -> String {
        bounded_preview(&SelectedValues { doc: self, ids })
    }
    pub fn search(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
    ) -> Result<Vec<usize>, String> {
        self.search_cancellable(term, case, regex, scope, || false)
    }
    pub fn search_cancellable(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
        cancelled: impl Fn() -> bool,
    ) -> Result<Vec<usize>, String> {
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = if regex {
            term.to_owned()
        } else {
            regex::escape(term)
        };
        let matcher = RegexBuilder::new(&pattern)
            .case_insensitive(!case)
            .build()
            .map_err(|e| format!("Invalid regular expression: {e}"))?;
        // Repeated object keys are tested once. Array keys contain only ASCII
        // digits, so a literal containing other characters cannot match them.
        let mut key_matches = Vec::new();
        if scope != SearchScope::Values {
            key_matches.reserve(self.keys.len());
            for (index, key) in self.keys.iter().enumerate() {
                if index % 1024 == 0 && cancelled() {
                    return Err("Search cancelled".into());
                }
                key_matches.push(matcher.is_match(key));
            }
        }
        let match_array_keys = regex || term.bytes().all(|byte| byte.is_ascii_digit());
        let root_match = scope != SearchScope::Values && matcher.is_match("$");
        let mut matches = Vec::new();
        let mut id = 0;
        let completed = walk(&self.value, &mut |value| {
            if id % 1024 == 0 && cancelled() {
                return false;
            }
            let node = &self.nodes[id];
            let key_match = scope != SearchScope::Values
                && match node.parent() {
                    None => root_match,
                    Some(parent) if matches!(self.nodes[parent].kind, Kind::Array) => {
                        match_array_keys && matcher.is_match(&node.segment.to_string())
                    }
                    Some(_) => key_matches[node.segment as usize],
                };
            let value_match = !key_match
                && scope != SearchScope::Keys
                && match value {
                    Value::String(s) => matcher.is_match(s),
                    Value::Number(n) => matcher.is_match(n.as_str()),
                    Value::Bool(b) => matcher.is_match(if *b { "true" } else { "false" }),
                    Value::Null => matcher.is_match("null"),
                    _ => false,
                };
            if key_match || value_match {
                matches.push(id);
            }
            id += 1;
            true
        });
        if completed {
            Ok(matches)
        } else {
            Err("Search cancelled".into())
        }
    }
    pub fn visible(&self, expanded: &[bool]) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut id = 0;
        while id < self.nodes.len() {
            rows.push(id);
            id = if self.nodes[id].container() && !expanded[id] {
                self.nodes[id].end()
            } else {
                id + 1
            };
        }
        rows
    }
}
struct SelectedValues<'a> {
    doc: &'a Document,
    ids: &'a [usize],
}
impl serde::Serialize for SelectedValues<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.ids.len()))?;
        for &id in self.ids {
            sequence.serialize_element(self.doc.at(id))?;
        }
        sequence.end()
    }
}
fn walk(value: &Value, visit: &mut impl FnMut(&Value) -> bool) -> bool {
    if !visit(value) {
        return false;
    }
    match value {
        Value::Object(map) => map.values().all(|child| walk(child, visit)),
        Value::Array(array) => array.iter().all(|child| walk(child, visit)),
        _ => true,
    }
}
#[derive(Clone, Copy, PartialEq)]
pub enum SearchScope {
    All,
    Keys,
    Values,
}
fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
fn short(text: &str, length: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(length).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}
// Bound serialization by output bytes, rather than by the size of the whole document.
fn bounded_preview<T: serde::Serialize>(value: &T) -> String {
    struct PreviewWriter {
        bytes: Vec<u8>,
        truncated: bool,
    }
    impl Write for PreviewWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let remaining = 200_000usize.saturating_sub(self.bytes.len());
            if buf.len() > remaining {
                self.bytes.extend_from_slice(&buf[..remaining]);
                self.truncated = true;
                return Err(io::Error::other("preview limit"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = PreviewWriter {
        bytes: Vec::new(),
        truncated: false,
    };
    let result = serde_json::to_writer_pretty(&mut writer, value);
    if result.is_err() && !writer.truncated {
        return "Cannot format preview".into();
    }
    let valid_len = match std::str::from_utf8(&writer.bytes) {
        Ok(_) => writer.bytes.len(),
        Err(error) => error.valid_up_to(),
    };
    let mut text = String::from_utf8(writer.bytes[..valid_len].to_vec()).unwrap();
    if writer.truncated {
        text.push_str("\n… Preview truncated. Copy or export for complete JSON.");
    }
    text
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_index_boundaries_and_root_sentinel() {
        assert_eq!(compact(0).unwrap(), 0);
        assert_eq!(compact(u32::MAX as usize).unwrap(), u32::MAX);
        if let Some(too_many) = (u32::MAX as usize).checked_add(1) {
            assert!(
                compact(too_many)
                    .unwrap_err()
                    .contains("32-bit index limit")
            );
        }
        let doc = Document::parse("[{},[],null]").unwrap();
        assert_eq!(doc.nodes[0].parent(), None);
        assert_eq!(doc.nodes[0].end(), doc.nodes.len());
        for id in 1..doc.nodes.len() {
            assert_eq!(doc.nodes[id].parent(), Some(0));
            assert_eq!(doc.nodes[id].end(), id + 1);
        }
        assert_eq!(
            doc.structure_capacity_bytes(),
            doc.nodes.len() * 28 + (doc.nodes.len() - 1) * 4
        );
    }
    #[test]
    fn compact_index_shares_keys_and_preserves_object_and_array_locations() {
        let d =
            Document::parse(r#"{"z":{"b":1,"a":2},"a":{"a":3,"b":4},"rows":[{"b":5},{"b":6}]}"#)
                .unwrap();
        assert_eq!(std::mem::size_of::<Node>(), 28);
        assert_eq!(std::mem::size_of_val(&d.children[0]), 4);
        assert_eq!(d.keys.len(), 4);
        assert_eq!(d.nodes.capacity(), d.nodes.len());
        for id in 0..d.nodes.len() {
            assert_eq!(d.value.pointer(&d.pointer(id)), Some(d.at(id)));
            assert_eq!(d.find_path(&d.path(id)).unwrap(), id);
        }
        assert_eq!(
            d.results_json(&d.query_path("$.rows[1,0,1].b").unwrap()),
            "[\n  6,\n  5,\n  6\n]"
        );
    }
    #[test]
    fn pointer_lookup_rejects_invalid_escapes_and_array_indices() {
        let d = Document::parse(r#"{"~2":1,"~":2,"a":[0,1]}"#).unwrap();
        assert_eq!(d.at(d.find_path("/~02").unwrap()), &serde_json::json!(1));
        for path in ["/~2", "/~", "/a/01", "/a/+1", "/a/-1", "/a/2"] {
            assert!(d.query_path(path).unwrap().is_empty(), "{path}");
        }
    }
    #[test]
    fn search_can_stop_before_scanning_the_document() {
        let d =
            Document::parse(r#"{"name":"你好","number":123456789012345678901234567890}"#).unwrap();
        assert!(
            d.search_cancellable("name", false, false, SearchScope::All, || true)
                .is_err()
        );
        assert_eq!(
            d.search("78901234567890", true, false, SearchScope::Values)
                .unwrap(),
            vec![2]
        );
        assert_eq!(
            d.search("你好", false, false, SearchScope::Values).unwrap(),
            vec![1]
        );
    }
    #[test]
    fn search_preserves_array_index_keys_regex_and_case_semantics() {
        let d = Document::parse(r#"[null,123,{"1":"你好","Email":"e"}]"#).unwrap();
        assert_eq!(
            d.search("1", true, false, SearchScope::Keys).unwrap(),
            vec![2, 4]
        );
        assert_eq!(
            d.search("^[0-9]$", true, true, SearchScope::Keys).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            d.search("1", true, false, SearchScope::Values).unwrap(),
            vec![2]
        );
        assert_eq!(
            d.search("email", false, false, SearchScope::All).unwrap(),
            vec![5]
        );
        assert!(
            d.search("email", true, false, SearchScope::All)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            d.search("你好", false, true, SearchScope::Values).unwrap(),
            vec![4]
        );
    }
    #[test]
    fn result_preview_stops_resolving_nodes_after_the_output_limit() {
        let d =
            Document::parse(&serde_json::json!({"large":"x".repeat(250_000)}).to_string()).unwrap();
        // The invalid trailing ID must never be visited after serialization is truncated.
        assert!(
            d.results_preview(&[1, usize::MAX])
                .contains("Preview truncated")
        );
    }
    #[test]
    fn paths_round_trip_special_keys_and_arrays() {
        let d = Document::parse(r#"{"a/b":{"~x":[{"a.b":"中文","quote\"":null}]},"":42}"#).unwrap();
        for id in 0..d.nodes.len() {
            assert_eq!(d.find_path(&d.path(id)).unwrap(), id);
            assert!(d.value.pointer(&d.pointer(id)).is_some());
        }
        assert_eq!(d.find_path("/a~1b/~0x/0/a.b").unwrap(), 4);
        assert!(d.find_path("$.missing").is_err());
        for invalid in ["$[01]", "$.a[", "$[?"] {
            assert!(d.query_path(invalid).is_err());
        }
    }
    #[test]
    fn search_checks_full_values_and_scopes() {
        let d = Document::parse(&format!(
            r#"{{"Email":"{}END","nested":{{"email":"yes"}},"n":null}}"#,
            "x".repeat(200)
        ))
        .unwrap();
        assert_eq!(
            d.search("email", false, false, SearchScope::Keys)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            d.search("email", true, false, SearchScope::Keys)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            d.search("END$", true, true, SearchScope::Values).unwrap(),
            vec![1]
        );
        assert!(d.search("[", false, true, SearchScope::All).is_err());
        assert_eq!(
            d.search("null", false, false, SearchScope::Values).unwrap(),
            vec![4]
        );
    }
    #[test]
    fn collapse_skips_whole_subtree() {
        let d = Document::parse(r#"{"a":[1,{"b":2}],"c":3}"#).unwrap();
        let mut expanded = vec![true; d.nodes.len()];
        expanded[1] = false;
        assert_eq!(d.visible(&expanded), vec![0, 1, 5]);
        expanded[0] = false;
        assert_eq!(d.visible(&expanded), vec![0]);
    }
    #[test]
    fn invalid_json_and_precision() {
        assert!(Document::parse("{bad}").err().unwrap().contains("line"));
        let d = Document::parse("\u{feff}{\"n\":123456789012345678901234567890}").unwrap();
        assert_eq!(d.at(1).to_string(), "123456789012345678901234567890");
    }
    #[test]
    fn jsonpath_queries_support_selectors_filters_and_exact_locations() {
        let d = Document::parse(r#"{"resultList":[{"ANS":"a","n":1},{"ANS":"b","n":2},{"ANS":"a","n":3},{"ANS":"韩国","n":4}],"a/b":{"x.y":true}}"#).unwrap();
        let values = |expr: &str| {
            d.query_path(expr)
                .unwrap()
                .iter()
                .map(|&id| d.at(id).clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values("$.resultList[3].ANS"),
            vec![serde_json::json!("韩国")]
        );
        assert_eq!(values("$.resultList[*].ANS").len(), 4);
        assert_eq!(values("$..ANS").len(), 4);
        assert_eq!(
            values("$.resultList[-1].ANS"),
            vec![serde_json::json!("韩国")]
        );
        assert_eq!(
            values("$.resultList[1:3].n"),
            vec![serde_json::json!(2), serde_json::json!(3)]
        );
        assert_eq!(
            values("$.resultList[?(@.n >= 3)].n"),
            vec![serde_json::json!(3), serde_json::json!(4)]
        );
        assert_eq!(values("$['a/b']['x.y']"), vec![serde_json::json!(true)]);
        let ids = d.query_path("$.resultList[0,2,0].ANS").unwrap();
        assert_ne!(ids[0], ids[1]);
        assert_eq!(ids[0], ids[2]);
        assert!(d.query_path("$.nothing").unwrap().is_empty());
        assert!(d.query_path("$[?").is_err());
        assert!(d.find_path("$.resultList[*]").is_err());
    }
    #[test]
    fn previews_are_bounded_by_selection_not_document_size() {
        let text = serde_json::json!({"small":"한국어", "large":"한".repeat(400_000)}).to_string();
        let d = Document::parse(&text).unwrap();
        assert!(d.bytes > 1_000_000);
        assert_eq!(d.preview(d.find_path("$.small").unwrap()), "\"한국어\"");
        let preview = d.preview(d.find_path("$.large").unwrap());
        assert!(preview.contains("Preview truncated"));
        assert!(!preview.contains('\u{fffd}'));
        assert!(preview.len() < 201_000);
        assert_eq!(d.results_preview(&[]), "[]");
    }
}

use regex::RegexBuilder;
use serde_json::Value;
use serde_json_path::JsonPath;
use std::{
    collections::HashMap,
    io::{self, Write},
};

pub struct Node {
    pub key: String,
    pub pointer: String,
    pub path: String,
    pub parent: Option<usize>,
    pub depth: usize,
    pub end: usize,
    pub kind: &'static str,
    pub preview: String,
    pub searchable: String,
    pub container: bool,
}
pub struct Document {
    pub value: Value,
    pub nodes: Vec<Node>,
    pub bytes: usize,
}

impl Document {
    pub fn parse(text: &str) -> Result<Self, String> {
        let value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        let mut doc = Self {
            value,
            nodes: Vec::new(),
            bytes: text.len(),
        };
        Self::index(
            &doc.value,
            &mut doc.nodes,
            "$".into(),
            String::new(),
            "$".into(),
            None,
            0,
        );
        Ok(doc)
    }
    fn index(
        value: &Value,
        nodes: &mut Vec<Node>,
        key: String,
        pointer: String,
        path: String,
        parent: Option<usize>,
        depth: usize,
    ) {
        let id = nodes.len();
        let (kind, preview, searchable, container) = match value {
            Value::Object(v) => (
                "object",
                format!("{{ {} keys }}", v.len()),
                String::new(),
                true,
            ),
            Value::Array(v) => (
                "array",
                format!("[ {} items ]", v.len()),
                String::new(),
                true,
            ),
            Value::String(s) => (
                "string",
                serde_json::to_string(&short(s, 160)).unwrap(),
                s.clone(),
                false,
            ),
            Value::Number(n) => ("number", n.to_string(), n.to_string(), false),
            Value::Bool(b) => ("boolean", b.to_string(), b.to_string(), false),
            Value::Null => ("null", "null".into(), "null".into(), false),
        };
        nodes.push(Node {
            key,
            pointer: pointer.clone(),
            path: path.clone(),
            parent,
            depth,
            end: id + 1,
            kind,
            preview,
            searchable,
            container,
        });
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let segment = key.replace('~', "~0").replace('/', "~1");
                    let child_path = if is_identifier(key) {
                        format!("{path}.{key}")
                    } else {
                        format!("{path}[{}]", serde_json::to_string(key).unwrap())
                    };
                    Self::index(
                        child,
                        nodes,
                        key.clone(),
                        format!("{pointer}/{segment}"),
                        child_path,
                        Some(id),
                        depth + 1,
                    );
                }
            }
            Value::Array(array) => {
                for (i, child) in array.iter().enumerate() {
                    Self::index(
                        child,
                        nodes,
                        i.to_string(),
                        format!("{pointer}/{i}"),
                        format!("{path}[{i}]"),
                        Some(id),
                        depth + 1,
                    );
                }
            }
            _ => (),
        }
        nodes[id].end = nodes.len();
    }
    pub fn at(&self, id: usize) -> &Value {
        self.value
            .pointer(&self.nodes[id].pointer)
            .expect("indexed pointer")
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
        let expression = expression.trim();
        if expression.starts_with('/') {
            return Ok(self
                .nodes
                .iter()
                .position(|n| n.pointer == expression)
                .into_iter()
                .collect());
        }
        let normalized = if expression == "." {
            "$".into()
        } else if expression.starts_with('.') || expression.starts_with('[') {
            format!("${expression}")
        } else {
            expression.to_owned()
        };
        let query = JsonPath::parse(&normalized).map_err(|e| format!("Invalid JSONPath: {e}"))?;
        let index: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(id, node)| (node.pointer.as_str(), id))
            .collect();
        query
            .query_located(&self.value)
            .iter()
            .map(|node| {
                let pointer = node.location().to_json_pointer();
                index
                    .get(pointer.as_str())
                    .copied()
                    .ok_or_else(|| "Query result not found in tree".into())
            })
            .collect()
    }
    pub fn preview(&self, id: usize) -> String {
        bounded_preview(self.at(id))
    }
    pub fn results_json(&self, ids: &[usize]) -> String {
        serde_json::to_string_pretty(&ids.iter().map(|&id| self.at(id)).collect::<Vec<_>>())
            .unwrap()
    }
    pub fn results_preview(&self, ids: &[usize]) -> String {
        bounded_preview(&ids.iter().map(|&id| self.at(id)).collect::<Vec<_>>())
    }
    pub fn search(
        &self,
        term: &str,
        case: bool,
        regex: bool,
        scope: SearchScope,
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
        Ok(self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                (scope != SearchScope::Values && matcher.is_match(&n.key))
                    || (scope != SearchScope::Keys
                        && !n.container
                        && matcher.is_match(&n.searchable))
            })
            .map(|(id, _)| id)
            .collect())
    }
    pub fn visible(&self, expanded: &[bool]) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut id = 0;
        while id < self.nodes.len() {
            rows.push(id);
            id = if self.nodes[id].container && !expanded[id] {
                self.nodes[id].end
            } else {
                id + 1
            };
        }
        rows
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
    fn paths_round_trip_special_keys_and_arrays() {
        let d = Document::parse(r#"{"a/b":{"~x":[{"a.b":"中文","quote\"":null}]},"":42}"#).unwrap();
        for (id, n) in d.nodes.iter().enumerate() {
            assert_eq!(d.find_path(&n.path).unwrap(), id);
            assert!(d.value.pointer(&n.pointer).is_some());
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

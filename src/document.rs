use regex::RegexBuilder;
use serde_json::Value;

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
        let pointer = path_to_pointer(path)?;
        self.nodes
            .iter()
            .position(|n| n.pointer == pointer)
            .ok_or_else(|| format!("Path not found: {path}"))
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
/// RFC 6901 pointers and a small JSONPath subset (no wildcard or expressions).
pub fn path_to_pointer(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.starts_with('/') {
        return Ok(input.into());
    }
    if matches!(input, "" | "$" | ".") {
        return Ok(String::new());
    }
    let input = input.strip_prefix('$').unwrap_or(input);
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut segments = Vec::new();
    while i < chars.len() {
        if chars[i] == '.' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                i += 1;
            }
            let key: String = chars[start..i].iter().collect();
            if !is_identifier(&key) {
                return Err("Use [\"key\"] for special property names".into());
            }
            segments.push(key);
        } else if chars[i] == '[' {
            i += 1;
            if i < chars.len() && chars[i] == '"' {
                let start = i;
                i += 1;
                let mut escaped = false;
                while i < chars.len() {
                    let c = chars[i];
                    i += 1;
                    if c == '"' && !escaped {
                        break;
                    }
                    escaped = c == '\\' && !escaped;
                }
                let quoted: String = chars[start..i].iter().collect();
                let key: String =
                    serde_json::from_str(&quoted).map_err(|_| "Invalid quoted key")?;
                segments.push(key);
            } else {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i {
                    return Err("Expected an array index or a double-quoted key".into());
                }
                let index: String = chars[start..i].iter().collect();
                if index.len() > 1 && index.starts_with('0') {
                    return Err("Array index must not have leading zeros".into());
                }
                segments.push(index);
            }
            if chars.get(i) != Some(&']') {
                return Err("Expected closing ]".into());
            }
            i += 1;
        } else {
            return Err("Use $.users[0].name, .users[0], or /users/0/name".into());
        }
    }
    Ok(segments
        .iter()
        .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
        .collect())
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
        for invalid in ["$[01]", "$.a[", "$..x", "$[*]"] {
            assert!(path_to_pointer(invalid).is_err());
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
}

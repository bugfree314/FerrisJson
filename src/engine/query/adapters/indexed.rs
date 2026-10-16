//! Existing Indexed selector grammar and collector, relocated without new syntax.
use crate::document::{Document, NodeIds};

#[derive(Clone)]
enum Selector {
    Name(String),
    Index(i64),
    Wildcard,
    Slice(Option<i64>, Option<i64>, i64),
}
fn unsupported() -> String {
    "Indexed JSONPath supports fields, unions, wildcard, recursion and slices; filters/functions are unsupported. Disable Indexed files and reopen for full JSONPath (no automatic full-document fallback).".into()
}
fn selectors(text: &str) -> Result<Vec<Selector>, String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escape = false;
    for (i, ch) in text.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == '\\' {
                escape = true;
            } else if ch == delimiter {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ',' {
            parts.push(&text[start..i]);
            start = i + 1;
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(|part| {
            let part = part.trim();
            if part == "*" {
                return Ok(Selector::Wildcard);
            }
            if part.starts_with('"') {
                return serde_json::from_str(part)
                    .map(Selector::Name)
                    .map_err(|_| unsupported());
            }
            if part.starts_with('\'') && part.ends_with('\'') {
                let mut encoded = String::from("\"");
                let mut chars = part[1..part.len() - 1].chars();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\\' => {
                            let next = chars.next().ok_or_else(unsupported)?;
                            if next == '\'' {
                                encoded.push('\'');
                            } else {
                                encoded.push('\\');
                                encoded.push(next);
                            }
                        }
                        '"' => encoded.push_str("\\\""),
                        _ => encoded.push(ch),
                    }
                }
                encoded.push('"');
                return serde_json::from_str(&encoded)
                    .map(Selector::Name)
                    .map_err(|_| unsupported());
            }
            if part.contains(':') {
                let parts: Vec<_> = part.split(':').collect();
                if !(2..=3).contains(&parts.len()) {
                    return Err(unsupported());
                }
                let number = |text: &str| {
                    if text.trim().is_empty() {
                        Ok(None)
                    } else {
                        text.trim()
                            .parse::<i64>()
                            .map(Some)
                            .map_err(|_| unsupported())
                    }
                };
                return Ok(Selector::Slice(
                    number(parts[0])?,
                    number(parts[1])?,
                    if parts.len() == 3 {
                        number(parts[2])?.unwrap_or(1)
                    } else {
                        1
                    },
                ));
            }
            part.parse().map(Selector::Index).map_err(|_| unsupported())
        })
        .collect()
}
struct Step {
    recursive: bool,
    selectors: Vec<Selector>,
}

pub(crate) struct Plan {
    steps: Vec<Step>,
}

pub(crate) fn compile(expression: &str, cancelled: &dyn Fn() -> bool) -> Result<Plan, String> {
    let mut segments = Vec::new();
    let bytes = expression.as_bytes();
    let mut position = 1;
    while position < bytes.len() {
        if cancelled() { return Err("Query cancelled".into()); }
        let mut recursive = false;
        if bytes[position] == b'.' {
            position += 1;
            if position < bytes.len() && bytes[position] == b'.' {
                recursive = true;
                position += 1;
            }
            if position >= bytes.len() {
                return Err(unsupported());
            }
            if bytes[position] != b'[' {
                let start = position;
                while position < bytes.len() && bytes[position] != b'.' && bytes[position] != b'[' {
                    position += 1;
                }
                let name = &expression[start..position];
                segments.push(Step {
                    recursive,
                    selectors: vec![if name == "*" {
                        Selector::Wildcard
                    } else {
                        Selector::Name(name.into())
                    }],
                });
                continue;
            }
        }
        if bytes[position] != b'[' {
            return Err(unsupported());
        }
        position += 1;
        let start = position;
        let mut quote = None;
        let mut escape = false;
        while position < bytes.len() {
            let byte = bytes[position];
            if escape {
                escape = false;
            } else if let Some(delimiter) = quote {
                if byte == b'\\' {
                    escape = true;
                } else if byte == delimiter {
                    quote = None;
                }
            } else if byte == b'\'' || byte == b'"' {
                quote = Some(byte);
            } else if byte == b']' {
                break;
            } else if byte == b'?' || byte == b'(' {
                return Err(unsupported());
            }
            position += 1;
        }
        if position == bytes.len() {
            return Err(unsupported());
        }
        segments.push(Step { recursive, selectors: selectors(&expression[start..position])? });
        position += 1;
    }
    Ok(Plan { steps: segments })
}

fn result_id(result: &mut NodeIds, id: usize) -> Result<(), String> {
    if result.len() == result.capacity() {
        result.reserve_exact(result.capacity().max(1024))?;
    }
    result.push(id as u32)?;
    Ok(())
}
pub(crate) fn execute(doc: &Document, plan: &Plan, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
    let mut current = NodeIds::new(doc.budget());
    current.push(0)?;
    for step in &plan.steps {
        let mut result = NodeIds::new(doc.budget());
        for &base in &current {
            let end = if step.recursive {
                doc.node_info(base as usize).end()
            } else {
                base as usize + 1
            };
            for id in base as usize..end {
                if cancelled() {
                    return Err("Query cancelled".into());
                }
                for selector in &step.selectors {
                    match selector {
                        Selector::Name(name) => {
                            if let Some(id) = doc.child_name(id, name) {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Wildcard => {
                            for child in doc.direct_children(id) {
                                if cancelled() {
                                    return Err("Query cancelled".into());
                                }
                                result_id(&mut result, child)?;
                            }
                        }
                        Selector::Index(index) => {
                            let len = doc.node_info(id).child_count.unwrap_or(0) as i64;
                            let index = if *index < 0 {
                                len.saturating_add(*index)
                            } else {
                                *index
                            };
                            if index >= 0
                                && let Some(id) = doc.child_index(id, index as usize)
                            {
                                result_id(&mut result, id)?;
                            }
                        }
                        Selector::Slice(start, end, step) => {
                            if doc.node_info(id).kind() != "array" || *step == 0 {
                                continue;
                            }
                            let len = doc.node_info(id).child_count.unwrap_or(0) as i64;
                            let normalize = |value: Option<i64>, default| {
                                value.map_or(default, |value| {
                                    if value < 0 {
                                        len.saturating_add(value)
                                    } else {
                                        value
                                    }
                                })
                            };
                            let (mut index, end) = if *step > 0 {
                                (
                                    normalize(*start, 0).clamp(0, len),
                                    normalize(*end, len).clamp(0, len),
                                )
                            } else {
                                (
                                    normalize(*start, len - 1).clamp(-1, len - 1),
                                    normalize(*end, -1).clamp(-1, len - 1),
                                )
                            };
                            while if *step > 0 { index < end } else { index > end } {
                                if cancelled() {
                                    return Err("Query cancelled".into());
                                }
                                result_id(
                                    &mut result,
                                    doc.child_index(id, index as usize).unwrap(),
                                )?;
                                let Some(next) = index.checked_add(*step) else {
                                    break;
                                };
                                index = next;
                            }
                        }
                    }
                }
            }
        }
        current = result;
    }
    Ok(current)
}

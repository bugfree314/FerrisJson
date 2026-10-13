//! Exact comparisons and bounded field reads for on-demand JSONPath filters.
use super::Document;
use serde_json::Value;
use std::cmp::Ordering;
#[derive(Clone)]
pub(super) enum Filter {
    Or(Box<Filter>, Box<Filter>),
    And(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
    Exists(String),
    Compare(String, String, String),
}
fn split<'a>(text: &'a str, operators: &[&str]) -> Option<(&'a str, &'a str, &'a str)> {
    let mut quote = None;
    let mut escape = false;
    let mut depth = 0;
    for (i, byte) in text.bytes().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if let Some(q) = quote {
            if byte == b'\\' {
                escape = true;
            } else if byte == q {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            for op in operators {
                if text.get(i..).is_some_and(|s| s.starts_with(op)) {
                    return Some((&text[..i], &text[i..i + op.len()], &text[i + op.len()..]));
                }
            }
        }
    }
    None
}
fn outer(text: &str) -> bool {
    if !text.starts_with('(') || !text.ends_with(')') {
        return false;
    }
    let mut depth = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, b) in text.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(q) = quote {
            if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'\'' | b'"' => quote = Some(b),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 && i + 1 < text.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}
impl Filter {
    pub(super) fn heap_bytes(&self) -> usize {
        match self {
            Self::Or(a, b) | Self::And(a, b) => {
                std::mem::size_of::<Self>() * 2 + a.heap_bytes() + b.heap_bytes()
            }
            Self::Not(a) => std::mem::size_of::<Self>() + a.heap_bytes(),
            Self::Exists(s) => s.capacity(),
            Self::Compare(a, op, b) => a.capacity() + op.capacity() + b.capacity(),
        }
    }
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 16 * 1024
            || text
                .bytes()
                .filter(|b| matches!(b, b'!' | b'&' | b'|' | b'('))
                .count()
                > 256
        {
            return Err("Filter complexity exceeds bounded planning budget".into());
        }
        let mut text = text.trim();
        while outer(text) {
            text = text[1..text.len() - 1].trim();
        }
        if let Some((a, _, b)) = split(text, &["||"]) {
            return Ok(Self::Or(
                Box::new(Self::parse(a)?),
                Box::new(Self::parse(b)?),
            ));
        }
        if let Some((a, _, b)) = split(text, &["&&"]) {
            return Ok(Self::And(
                Box::new(Self::parse(a)?),
                Box::new(Self::parse(b)?),
            ));
        }
        if let Some(rest) = text.strip_prefix('!') {
            return Ok(Self::Not(Box::new(Self::parse(rest)?)));
        }
        if let Some((a, op, b)) = split(text, &["==", "!=", "<=", ">=", "<", ">"]) {
            for operand in [a.trim(), b.trim()] {
                if operand.starts_with('@') || operand.starts_with('$') {
                    continue;
                }
                if operand.starts_with('\'') {
                    super::indexed::selectors(operand)?;
                } else {
                    serde_json::from_str::<Value>(operand)
                        .map_err(|_| "Unsupported filter operand or function".to_string())?;
                }
            }
            return Ok(Self::Compare(a.trim().into(), op.into(), b.trim().into()));
        }
        if text.starts_with('@') || text.starts_with('$') {
            return Ok(Self::Exists(text.into()));
        }
        Err("On-demand filters support existence, comparisons, !, && and ||; functions are unsupported".into())
    }
    fn path(
        doc: &Document,
        id: usize,
        text: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<u32>, String> {
        let path = if let Some(relative) = text.strip_prefix('@') {
            format!("{}{relative}", doc.try_path(id)?)
        } else {
            text.to_owned()
        };
        doc.query_path_cancellable(&path, cancel)
    }
    fn value(
        doc: &Document,
        id: usize,
        text: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<Value>, String> {
        if text.starts_with('@') || text.starts_with('$') {
            return match Self::path(doc, id, text, cancel)?.as_slice() {
                [] => Ok(None),
                [id] => doc.read_subtree(*id as usize).map(|v| Some((*v).clone())),
                _ => Err("Comparison operands must select one value".into()),
            };
        }
        if text.starts_with('\'')
            && let super::indexed::Selector::Name(name) = super::indexed::selectors(text)?.remove(0)
        {
            return Ok(Some(Value::String(name)));
        }
        serde_json::from_str(text)
            .map(Some)
            .map_err(|_| "Unsupported filter operand or function".into())
    }
    pub(super) fn evaluate(
        &self,
        doc: &Document,
        id: usize,
        cancel: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        if cancel() {
            return Err("Query cancelled".into());
        }
        match self {
            Self::Or(a, b) => Ok(a.evaluate(doc, id, cancel)? || b.evaluate(doc, id, cancel)?),
            Self::And(a, b) => Ok(a.evaluate(doc, id, cancel)? && b.evaluate(doc, id, cancel)?),
            Self::Not(a) => Ok(!a.evaluate(doc, id, cancel)?),
            Self::Exists(path) => Ok(!Self::path(doc, id, path, cancel)?.is_empty()),
            Self::Compare(a, op, b) => {
                let a = Self::value(doc, id, a, cancel)?;
                let b = Self::value(doc, id, b, cancel)?;
                let ordered = match (&a, &b) {
                    (Some(Value::Number(a)), Some(Value::Number(b))) => {
                        Some(number_cmp(a.as_str(), b.as_str())?)
                    }
                    (Some(Value::String(a)), Some(Value::String(b))) => Some(a.cmp(b)),
                    _ => None,
                };
                let equal = ordered.map_or(a == b, |o| o == Ordering::Equal);
                Ok(match op.as_str() {
                    "==" => equal,
                    "!=" => !equal,
                    "<" => ordered == Some(Ordering::Less),
                    ">" => ordered == Some(Ordering::Greater),
                    "<=" => ordered.is_some_and(|o| o != Ordering::Greater),
                    ">=" => ordered.is_some_and(|o| o != Ordering::Less),
                    _ => false,
                })
            }
        }
    }
}
fn number_cmp(a: &str, b: &str) -> Result<Ordering, String> {
    fn normalize(text: &str) -> Result<(bool, String, i128), String> {
        let negative = text.starts_with('-');
        let text = text.trim_start_matches('-');
        let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
        let exponent: i128 = exponent
            .parse()
            .map_err(|_| "Numeric exponent exceeds exact comparison range")?;
        let decimals = mantissa.split_once('.').map_or(0, |(_, s)| s.len());
        let digits = mantissa.replace('.', "");
        let digits = digits.trim_start_matches('0').to_owned();
        if digits.is_empty() {
            return Ok((false, "0".into(), 0));
        }
        let magnitude = exponent
            .checked_sub(decimals as i128)
            .and_then(|e| e.checked_add(digits.len() as i128))
            .ok_or("Numeric exponent exceeds exact comparison range")?;
        Ok((negative, digits, magnitude))
    }
    let (an, a, am) = normalize(a)?;
    let (bn, b, bm) = normalize(b)?;
    if a == "0" && b == "0" {
        return Ok(Ordering::Equal);
    }
    if a == "0" {
        return Ok(if bn {
            Ordering::Greater
        } else {
            Ordering::Less
        });
    }
    if b == "0" {
        return Ok(if an {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    if an != bn {
        return Ok(if an {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let order = am.cmp(&bm).then_with(|| {
        (0..a.len().max(b.len()))
            .map(|i| {
                a.as_bytes()
                    .get(i)
                    .copied()
                    .unwrap_or(b'0')
                    .cmp(&b.as_bytes().get(i).copied().unwrap_or(b'0'))
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    });
    Ok(if an { order.reverse() } else { order })
}

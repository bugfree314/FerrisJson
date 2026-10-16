use super::syntax::{Compare, Operand, Part, Path, Predicate};
use crate::engine::tree::{Provider, TreeNode};
use serde_json::Value;
use std::{cmp::Ordering, io::{self, Write}};

pub const SCALAR_LIMIT: usize = 256 * 1024;
pub fn index(index: i64, count: u64) -> Option<u64> {
    let n = if index < 0 { i128::from(count) + i128::from(index) } else { i128::from(index) };
    (n >= 0 && n < i128::from(count)).then_some(n as u64)
}
fn locate(provider: &mut dyn Provider, path: &Path, current: TreeNode, root: TreeNode, cancel: &dyn Fn() -> bool) -> Result<Option<TreeNode>, String> {
    let mut node = if path.root { root } else { current };
    for part in &path.parts {
        if cancel() { return Err("Query cancelled".into()); }
        node = match part {
            Part::Name(name) => match provider.named_child(node, name, cancel)? { Some((_, n)) => n, None => return Ok(None) },
            Part::Index(i) if node.kind == "array" => match index(*i, node.children) {
                Some(i) => provider.child(node, i, cancel)?, None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(node))
}

struct Buffer(Vec<u8>);
impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > SCALAR_LIMIT {
            return Err(io::Error::other("Filter scalar exceeds 256 KiB evaluation workspace"));
        }
        self.0.extend_from_slice(bytes); Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
fn operand(provider: &mut dyn Provider, value: &Operand, current: TreeNode, root: TreeNode, cancel: &dyn Fn() -> bool) -> Result<Option<Value>, String> {
    match value {
        Operand::Literal(value) => Ok(Some(value.clone())),
        Operand::Path(path) => {
            let Some(node) = locate(provider, path, current, root, cancel)? else { return Ok(None); };
            if node.container() { return Err("Streaming filter container comparison is unsupported; use a scalar operand".into()); }
            let mut bytes = Buffer(Vec::with_capacity(SCALAR_LIMIT));
            provider.output(node, cancel)?.write(&mut bytes, cancel).map_err(|e| e.to_string())?;
            Ok(Some(serde_json::from_slice(&bytes.0).map_err(|e| format!("Filter scalar decode: {e}"))?))
        }
    }
}
struct Decimal { sign: i8, power: i128, digits: String }
fn decimal(text: &str) -> Result<Decimal, String> {
    let negative = text.starts_with('-');
    let text = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = if let Some(i) = text.find(['e', 'E']) {
        (&text[..i], text[i + 1..].parse::<i128>().map_err(|_| "Filter decimal exponent exceeds supported integer range")?)
    } else { (text, 0) };
    let integer = mantissa.find('.').unwrap_or(mantissa.len());
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let leading = digits.bytes().take_while(|b| *b == b'0').count();
    if leading == digits.len() { return Ok(Decimal { sign: 0, power: 0, digits: String::new() }); }
    let power = exponent.checked_add(integer as i128).and_then(|v| v.checked_sub(leading as i128)).ok_or("Filter decimal exponent overflow")?;
    Ok(Decimal { sign: if negative { -1 } else { 1 }, power, digits: digits[leading..].trim_end_matches('0').into() })
}
fn number_order(left: &str, right: &str) -> Result<Ordering, String> {
    let (left, right) = (decimal(left)?, decimal(right)?);
    if left.sign != right.sign { return Ok(left.sign.cmp(&right.sign)); }
    if left.sign == 0 { return Ok(Ordering::Equal); }
    let mut order = left.power.cmp(&right.power);
    if order == Ordering::Equal {
        let width = left.digits.len().max(right.digits.len());
        order = left.digits.bytes().chain(std::iter::repeat(b'0')).take(width)
            .cmp(right.digits.bytes().chain(std::iter::repeat(b'0')).take(width));
    }
    Ok(if left.sign < 0 { order.reverse() } else { order })
}
fn compare(left: Option<Value>, op: Compare, right: Option<Value>) -> Result<bool, String> {
    let (equal, order) = match (&left, &right) {
        (None, None) => (true, None),
        (Some(Value::Number(a)), Some(Value::Number(b))) => {
            let order = number_order(a.as_str(), b.as_str())?; (order == Ordering::Equal, Some(order))
        }
        (Some(Value::String(a)), Some(Value::String(b))) => (a == b, Some(a.cmp(b))),
        (Some(Value::Bool(a)), Some(Value::Bool(b))) => (a == b, None),
        (Some(Value::Null), Some(Value::Null)) => (true, None),
        _ => (false, None),
    };
    Ok(match op {
        Compare::Eq => equal, Compare::Ne => !equal,
        Compare::Lt => order == Some(Ordering::Less), Compare::Gt => order == Some(Ordering::Greater),
        Compare::Le => equal || order == Some(Ordering::Less), Compare::Ge => equal || order == Some(Ordering::Greater),
    })
}
pub fn evaluate(provider: &mut dyn Provider, filter: &Predicate, current: TreeNode, root: TreeNode, cancel: &dyn Fn() -> bool) -> Result<bool, String> {
    if cancel() { return Err("Query cancelled".into()); }
    match filter {
        Predicate::Exists(path) => Ok(locate(provider, path, current, root, cancel)?.is_some()),
        Predicate::Not(p) => Ok(!evaluate(provider, p, current, root, cancel)?),
        Predicate::And(a, b) => Ok(evaluate(provider, a, current, root, cancel)? && evaluate(provider, b, current, root, cancel)?),
        Predicate::Or(a, b) => Ok(evaluate(provider, a, current, root, cancel)? || evaluate(provider, b, current, root, cancel)?),
        Predicate::Compare(a, op, b) => {
            let a = operand(provider, a, current, root, cancel)?;
            let b = operand(provider, b, current, root, cancel)?;
            compare(a, *op, b)
        }
    }
}

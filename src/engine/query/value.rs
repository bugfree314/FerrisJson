use super::plan::Op;
use serde_json::Value;
use std::cmp::Ordering;
struct Decimal {
    negative: bool,
    digits: String,
    exponent: i64,
}
impl Decimal {
    fn parse(s: &str) -> Result<Self, String> {
        let negative = s.starts_with('-');
        let s = s.strip_prefix('-').unwrap_or(s);
        let (mantissa, exponent) = s.split_once(['e', 'E']).unwrap_or((s, "0"));
        let exponent = exponent
            .parse::<i64>()
            .map_err(|_| "Numeric exponent exceeds i64 query limit")?;
        let fraction = mantissa.split_once('.').map_or(0, |(_, f)| f.len());
        let digits = mantissa.chars().filter(|c| *c != '.').collect::<String>();
        let digits = digits.trim_start_matches('0');
        let significant = digits.trim_end_matches('0');
        let exponent = exponent
            .checked_sub(fraction as i64)
            .and_then(|e| e.checked_add((digits.len() - significant.len()) as i64))
            .ok_or("Numeric exponent exceeds i64 query limit")?;
        Ok(Self {
            negative: negative && !significant.is_empty(),
            digits: significant.into(),
            exponent,
        })
    }
    fn cmp(&self, other: &Self) -> Ordering {
        if self.negative != other.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let a = if self.digits.is_empty() {
            i128::MIN
        } else {
            self.digits.len() as i128 + self.exponent as i128
        };
        let b = if other.digits.is_empty() {
            i128::MIN
        } else {
            other.digits.len() as i128 + other.exponent as i128
        };
        let order = a.cmp(&b).then_with(|| {
            self.digits
                .bytes()
                .chain(std::iter::repeat(b'0'))
                .take(self.digits.len().max(other.digits.len()))
                .cmp(
                    other
                        .digits
                        .bytes()
                        .chain(std::iter::repeat(b'0'))
                        .take(self.digits.len().max(other.digits.len())),
                )
        });
        if self.negative {
            order.reverse()
        } else {
            order
        }
    }
}
pub(super) fn compare(a: Option<Value>, op: Op, b: Option<Value>) -> Result<bool, String> {
    let order = match (&a, &b) {
        (None, None) => Some(Ordering::Equal),
        (Some(Value::Null), Some(Value::Null)) => Some(Ordering::Equal),
        (Some(Value::Bool(a)), Some(Value::Bool(b))) => {
            if a == b {
                Some(Ordering::Equal)
            } else {
                None
            }
        }
        (Some(Value::String(a)), Some(Value::String(b))) => Some(a.cmp(b)),
        (Some(Value::Number(a)), Some(Value::Number(b))) => {
            Some(Decimal::parse(a.as_str())?.cmp(&Decimal::parse(b.as_str())?))
        }
        _ => None,
    };
    let ordered = matches!(
        (&a, &b),
        (Some(Value::String(_)), Some(Value::String(_)))
            | (Some(Value::Number(_)), Some(Value::Number(_)))
    );
    Ok(match op {
        Op::Eq => order == Some(Ordering::Equal),
        Op::Ne => order != Some(Ordering::Equal),
        Op::Lt => ordered && order == Some(Ordering::Less),
        Op::Le => order == Some(Ordering::Equal) || (ordered && order == Some(Ordering::Less)),
        Op::Gt => ordered && order == Some(Ordering::Greater),
        Op::Ge => order == Some(Ordering::Equal) || (ordered && order == Some(Ordering::Greater)),
    })
}

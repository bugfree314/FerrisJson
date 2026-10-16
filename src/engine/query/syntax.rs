//! Explicit streaming subset, paired with library validation of RFC syntax.
use serde_json::Value;

pub const DEPTH: usize = 128;

pub struct Plan { pub steps: Vec<Step>, pub filters: bool }
pub struct Step { pub recursive: bool, pub selectors: Vec<Selector> }
pub enum Selector {
    Name(String), Index(i64), Wildcard, Slice(Option<i64>, Option<i64>, i64), Filter(Predicate),
}
pub enum Part { Name(String), Index(i64) }
pub struct Path { pub root: bool, pub parts: Vec<Part> }
pub enum Operand { Path(Path), Literal(Value) }
#[derive(Clone, Copy)]
pub enum Compare { Eq, Ne, Lt, Le, Gt, Ge }
pub enum Predicate {
    Exists(Path), Compare(Operand, Compare, Operand),
    Not(Box<Predicate>), And(Box<Predicate>, Box<Predicate>), Or(Box<Predicate>, Box<Predicate>),
}

#[derive(PartialEq)]
enum Token {
    Root, Current, Dot, Recursive, Open, Close, Left, Right, Comma, Colon, Star, Question,
    Not, And, Or, Eq, Ne, Lt, Le, Gt, Ge, Name(String), String(String), Number(String), End,
}
struct Lexer<'a> { source: &'a str, pos: usize }
fn unsupported() -> String {
    "Streaming JSONPath supports selectors and scalar filters with singular @/$ paths, comparisons, !, && and ||; functions, non-singular filter operands and container comparisons are unsupported".into()
}
impl Lexer<'_> {
    fn token(&mut self) -> Result<Token, String> {
        let bytes = self.source.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() { self.pos += 1; }
        if self.pos == bytes.len() { return Ok(Token::End); }
        let start = self.pos;
        let byte = bytes[self.pos]; self.pos += 1;
        Ok(match byte {
            b'$' => Token::Root, b'@' => Token::Current,
            b'.' if bytes.get(self.pos) == Some(&b'.') => { self.pos += 1; Token::Recursive }
            b'.' => Token::Dot, b'[' => Token::Open, b']' => Token::Close,
            b'(' => Token::Left, b')' => Token::Right, b',' => Token::Comma,
            b':' => Token::Colon, b'*' => Token::Star, b'?' => Token::Question,
            b'!' if bytes.get(self.pos) == Some(&b'=') => { self.pos += 1; Token::Ne }
            b'!' => Token::Not,
            b'=' if bytes.get(self.pos) == Some(&b'=') => { self.pos += 1; Token::Eq }
            b'&' if bytes.get(self.pos) == Some(&b'&') => { self.pos += 1; Token::And }
            b'|' if bytes.get(self.pos) == Some(&b'|') => { self.pos += 1; Token::Or }
            b'<' if bytes.get(self.pos) == Some(&b'=') => { self.pos += 1; Token::Le }
            b'>' if bytes.get(self.pos) == Some(&b'=') => { self.pos += 1; Token::Ge }
            b'<' => Token::Lt, b'>' => Token::Gt,
            b'\'' | b'"' => {
                let mut escaped = false;
                while self.pos < bytes.len() {
                    let next = bytes[self.pos]; self.pos += 1;
                    if escaped { escaped = false; }
                    else if next == b'\\' { escaped = true; }
                    else if next == byte { break; }
                }
                if self.pos <= start + 1 || bytes.get(self.pos - 1) != Some(&byte) { return Err(unsupported()); }
                let raw = &self.source[start..self.pos];
                let value = if byte == b'"' { serde_json::from_str(raw).map_err(|_| unsupported())? }
                else {
                    let mut encoded = String::from("\"");
                    let mut chars = raw[1..raw.len() - 1].chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => {
                                let next = chars.next().ok_or_else(unsupported)?;
                                if next == '\'' { encoded.push('\''); }
                                else { encoded.push('\\'); encoded.push(next); }
                            }
                            '"' => encoded.push_str("\\\""), _ => encoded.push(c),
                        }
                    }
                    encoded.push('"');
                    serde_json::from_str(&encoded).map_err(|_| unsupported())?
                };
                Token::String(value)
            }
            b'-' | b'0'..=b'9' => {
                while self.pos < bytes.len() && matches!(bytes[self.pos], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') { self.pos += 1; }
                Token::Number(self.source[start..self.pos].into())
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 128 => {
                while self.pos < bytes.len() && (bytes[self.pos].is_ascii_alphanumeric() || bytes[self.pos] == b'_' || bytes[self.pos] >= 128) { self.pos += 1; }
                Token::Name(self.source[start..self.pos].into())
            }
            _ => return Err(unsupported()),
        })
    }
}
struct Parser<'a> { lexer: Lexer<'a>, token: Token, depth: usize, logical: usize, filters: bool, cancel: &'a dyn Fn() -> bool }
impl Parser<'_> {
    fn bump(&mut self) -> Result<Token, String> {
        if (self.cancel)() { return Err("Query cancelled".into()); }
        let next = self.lexer.token()?;
        Ok(std::mem::replace(&mut self.token, next))
    }
    fn take(&mut self, expected: Token) -> Result<(), String> {
        if self.token != expected { return Err(unsupported()); }
        self.bump()?; Ok(())
    }
    fn integer(&mut self) -> Result<i64, String> {
        match self.bump()? { Token::Number(n) => n.parse().map_err(|_| unsupported()), _ => Err(unsupported()) }
    }
    fn bracket(&mut self) -> Result<Vec<Selector>, String> {
        self.take(Token::Open)?;
        let mut selectors = Vec::new();
        loop {
            let selector = match &self.token {
                Token::Question => { self.bump()?; self.filters = true; Selector::Filter(self.or()?) }
                Token::String(_) => match self.bump()? { Token::String(n) => Selector::Name(n), _ => unreachable!() },
                Token::Star => { self.bump()?; Selector::Wildcard }
                Token::Colon | Token::Number(_) => {
                    let start = if self.token == Token::Colon { None } else { Some(self.integer()?) };
                    if self.token != Token::Colon { Selector::Index(start.ok_or_else(unsupported)?) }
                    else {
                        self.bump()?;
                        let end = if matches!(self.token, Token::Number(_)) { Some(self.integer()?) } else { None };
                        let step = if self.token == Token::Colon {
                            self.bump()?;
                            if matches!(self.token, Token::Number(_)) { self.integer()? } else { 1 }
                        } else { 1 };
                        Selector::Slice(start, end, step)
                    }
                }
                _ => return Err(unsupported()),
            };
            selectors.push(selector);
            if self.token != Token::Comma { break; }
            self.bump()?;
        }
        self.take(Token::Close)?; Ok(selectors)
    }
    fn path(&mut self) -> Result<Path, String> {
        let root = match self.bump()? { Token::Root => true, Token::Current => false, _ => return Err(unsupported()) };
        let mut parts = Vec::new();
        loop {
            if parts.len() >= DEPTH && matches!(self.token, Token::Dot | Token::Open) {
                return Err("Query path depth exceeds 128".into());
            }
            match self.token {
                Token::Dot => {
                    self.bump()?;
                    match self.bump()? { Token::Name(n) => parts.push(Part::Name(n)), _ => return Err(unsupported()) }
                }
                Token::Open => {
                    self.bump()?;
                    let part = match &self.token {
                        Token::Number(_) => Part::Index(self.integer()?),
                        Token::String(_) => match self.bump()? { Token::String(n) => Part::Name(n), _ => unreachable!() },
                        _ => return Err(unsupported()),
                    };
                    self.take(Token::Close)?; parts.push(part);
                }
                _ => break,
            }
        }
        Ok(Path { root, parts })
    }
    fn operand(&mut self) -> Result<Operand, String> {
        if matches!(self.token, Token::Root | Token::Current) { return Ok(Operand::Path(self.path()?)); }
        let value = match self.bump()? {
            Token::String(s) => Value::String(s),
            Token::Number(n) => serde_json::from_str(&n).map_err(|_| unsupported())?,
            Token::Name(n) if n == "true" => Value::Bool(true),
            Token::Name(n) if n == "false" => Value::Bool(false),
            Token::Name(n) if n == "null" => Value::Null,
            _ => return Err(unsupported()),
        };
        Ok(Operand::Literal(value))
    }
    fn atom(&mut self) -> Result<Predicate, String> {
        self.depth += 1;
        if self.depth >= DEPTH { return Err("Query filter nesting exceeds 127".into()); }
        let result = if self.token == Token::Not { self.bump()?; Predicate::Not(Box::new(self.atom()?)) }
        else if self.token == Token::Left { self.bump()?; let p = self.or()?; self.take(Token::Right)?; p }
        else {
            let left = self.operand()?;
            let op = match self.token {
                Token::Eq => Some(Compare::Eq), Token::Ne => Some(Compare::Ne),
                Token::Lt => Some(Compare::Lt), Token::Le => Some(Compare::Le),
                Token::Gt => Some(Compare::Gt), Token::Ge => Some(Compare::Ge), _ => None,
            };
            if let Some(op) = op { self.bump()?; Predicate::Compare(left, op, self.operand()?) }
            else if let Operand::Path(path) = left { Predicate::Exists(path) }
            else { return Err(unsupported()); }
        };
        self.depth -= 1; Ok(result)
    }
    fn and(&mut self) -> Result<Predicate, String> {
        let mut p = self.atom()?;
        while self.token == Token::And {
            self.logical += 1;
            if self.logical >= DEPTH { return Err("Query exceeds 127 logical operators".into()); }
            self.bump()?; p = Predicate::And(Box::new(p), Box::new(self.atom()?));
        }
        Ok(p)
    }
    fn or(&mut self) -> Result<Predicate, String> {
        let mut p = self.and()?;
        while self.token == Token::Or {
            self.logical += 1;
            if self.logical >= DEPTH { return Err("Query exceeds 127 logical operators".into()); }
            self.bump()?; p = Predicate::Or(Box::new(p), Box::new(self.and()?));
        }
        Ok(p)
    }
}

pub fn compile(expression: &str, cancel: &dyn Fn() -> bool) -> Result<Plan, String> {
    let mut parser = Parser { lexer: Lexer { source: expression, pos: 0 }, token: Token::End, depth: 0, logical: 0, filters: false, cancel };
    parser.token = parser.lexer.token()?;
    parser.take(Token::Root)?;
    let mut steps = Vec::new();
    while parser.token != Token::End {
        if steps.len() >= DEPTH { return Err("Query exceeds 128 steps".into()); }
        let recursive = parser.token == Token::Recursive;
        let selectors = match parser.token {
            Token::Dot | Token::Recursive => {
                parser.bump()?;
                match parser.token {
                    Token::Open => parser.bracket()?,
                    Token::Star => { parser.bump()?; vec![Selector::Wildcard] }
                    _ => match parser.bump()? { Token::Name(n) => vec![Selector::Name(n)], _ => return Err(unsupported()) },
                }
            }
            Token::Open => parser.bracket()?,
            _ => return Err(unsupported()),
        };
        steps.push(Step { recursive, selectors });
    }
    Ok(Plan { steps, filters: parser.filters })
}

//! Explicit bounded JSONPath subset. No delimiter splitting of expressions.
#[derive(Clone, Debug)]
pub struct Plan {
    pub root: bool,
    pub steps: Vec<Step>,
}
#[derive(Clone, Debug)]
pub struct Step {
    pub recursive: bool,
    pub selectors: Vec<Selector>,
}
#[derive(Clone, Debug)]
pub enum Selector {
    Name(String),
    Index(i64),
    Wildcard,
    Slice(Option<i64>, Option<i64>, i64),
    Filter(Expr),
    Pointer(String),
}
#[derive(Clone, Debug)]
pub enum Expr {
    Exists(Plan),
    Compare(Operand, Op, Operand),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}
#[derive(Clone, Debug)]
pub enum Operand {
    Path(Plan),
    Literal(serde_json::Value),
}
#[derive(Clone, Copy, Debug)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Symbol(char),
    Descend,
    Text(String),
    Ident(String),
    Number(String),
    Eq,
    Ne,
    Le,
    Ge,
    And,
    Or,
    End,
}
struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
    depth: usize,
}
impl Plan {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 64 * 1024 {
            return Err("JSONPath expression exceeds 64 KiB".into());
        }
        let text = text.trim();
        if let Some(pointer) = text.strip_prefix('/') {
            let mut steps = Vec::new();
            for segment in pointer.split('/') {
                let mut name = String::new();
                let mut chars = segment.chars();
                while let Some(c) = chars.next() {
                    name.push(if c == '~' {
                        match chars.next() {
                            Some('0') => '~',
                            Some('1') => '/',
                            _ => return Err("Invalid Pointer escape".into()),
                        }
                    } else {
                        c
                    });
                }
                steps.push(Step {
                    recursive: false,
                    selectors: vec![Selector::Pointer(name)],
                });
                if steps.len() > 64 {
                    return Err("Query exceeds 64 segments".into());
                }
            }
            return Ok(Self { root: true, steps });
        }
        let normalized = if text == "." {
            "$".into()
        } else if text.starts_with(['.', '[']) {
            format!("${text}")
        } else {
            text.to_owned()
        };
        let mut parser = Parser {
            tokens: lex(&normalized)?,
            pos: 0,
            depth: 0,
        };
        let plan = parser.path()?;
        if !plan.root {
            return Err(parser.error("Top-level query must start with $"));
        }
        if parser.peek() != &Token::End {
            return Err(parser.error("Unexpected token / unsupported function"));
        }
        validate(&plan, 0)?;
        Ok(plan)
    }
    pub fn compatibility_guard(text: &str, nodes: usize) -> Result<(), String> {
        if text.len() > 64 * 1024 {
            return Err("Compatibility query exceeds 64 KiB".into());
        }
        let tokens = lex(text)?;
        let mut depth = 0usize;
        let mut expansion = 1usize;
        for (token, _) in tokens {
            match token {
                Token::Symbol('(' | '[') => {
                    depth += 1;
                    if depth > 32 {
                        return Err("Compatibility query nesting exceeds 32".into());
                    }
                }
                Token::Symbol(')' | ']') => depth = depth.saturating_sub(1),
                Token::Symbol('*' | '?') | Token::Descend => {
                    expansion = expansion.saturating_mul(nodes.max(1));
                }
                Token::Symbol(',') => expansion = expansion.saturating_mul(64),
                _ => {}
            }
            if expansion > 65_536 {
                return Err("Full-library compatibility query exceeds conservative expansion budget; use the streaming subset or narrow the expression".into());
            }
        }
        Ok(())
    }
    pub fn singular(&self) -> bool {
        self.steps.iter().all(|s| {
            !s.recursive
                && s.selectors.len() == 1
                && matches!(
                    s.selectors[0],
                    Selector::Name(_) | Selector::Index(_) | Selector::Pointer(_)
                )
        })
    }
}
fn lex(text: &str) -> Result<Vec<(Token, usize)>, String> {
    let mut tokens = Vec::new();
    let mut p = 0;
    while p < text.len() {
        let c = text[p..].chars().next().unwrap();
        if c.is_whitespace() {
            p += c.len_utf8();
            continue;
        }
        let start = p;
        p += c.len_utf8();
        let token = if matches!(c, '\'' | '"') {
            let quote = c;
            let mut encoded = String::from("\"");
            let mut closed = false;
            while p < text.len() {
                let ch = text[p..].chars().next().unwrap();
                p += ch.len_utf8();
                if ch == quote {
                    closed = true;
                    break;
                }
                if ch == '\\' {
                    let next = text[p..]
                        .chars()
                        .next()
                        .ok_or_else(|| format!("Unclosed escape at byte {p}"))?;
                    p += next.len_utf8();
                    if quote == '\'' && next == '\'' {
                        encoded.push('\'');
                    } else {
                        encoded.push('\\');
                        encoded.push(next);
                    }
                } else if ch == '"' {
                    encoded.push_str("\\\"");
                } else {
                    encoded.push(ch);
                }
            }
            if !closed {
                return Err(format!("Unclosed string at byte {start}"));
            }
            encoded.push('"');
            Token::Text(
                serde_json::from_str(&encoded)
                    .map_err(|e| format!("Invalid string at byte {start}: {e}"))?,
            )
        } else if c == '-' || c.is_ascii_digit() {
            while p < text.len()
                && matches!(
                    text.as_bytes()[p],
                    b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                )
            {
                p += 1;
            }
            let n = &text[start..p];
            if !matches!(
                serde_json::from_str::<serde_json::Value>(n),
                Ok(serde_json::Value::Number(_))
            ) {
                return Err(format!("Invalid number at byte {start}"));
            }
            Token::Number(n.into())
        } else if c.is_ascii_alphabetic() || c == '_' || c as u32 >= 0x80 {
            while p < text.len() {
                let ch = text[p..].chars().next().unwrap();
                if !(ch.is_ascii_alphanumeric() || ch == '_' || ch as u32 >= 0x80) {
                    break;
                }
                p += ch.len_utf8();
            }
            Token::Ident(text[start..p].into())
        } else {
            let next = text.as_bytes().get(p).copied();
            let pair = match (c, next) {
                ('.', Some(b'.')) => Some(Token::Descend),
                ('=', Some(b'=')) => Some(Token::Eq),
                ('!', Some(b'=')) => Some(Token::Ne),
                ('<', Some(b'=')) => Some(Token::Le),
                ('>', Some(b'=')) => Some(Token::Ge),
                ('&', Some(b'&')) => Some(Token::And),
                ('|', Some(b'|')) => Some(Token::Or),
                _ => None,
            };
            if let Some(t) = pair {
                p += 1;
                t
            } else if "$@.[],:*()?<>!".contains(c) {
                Token::Symbol(c)
            } else {
                return Err(format!("Unsupported character at byte {start}"));
            }
        };
        if tokens.len() >= 2048 {
            return Err(format!("Query exceeds 2048 tokens at byte {start}"));
        }
        tokens.push((token, start));
    }
    tokens.push((Token::End, text.len()));
    Ok(tokens)
}
impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].0
    }
    fn take(&mut self) -> Token {
        let token = self.peek().clone();
        if token != Token::End {
            self.pos += 1;
        }
        token
    }
    fn error(&self, message: &str) -> String {
        format!("JSONPath at byte {}: {message}", self.tokens[self.pos].1)
    }
    fn symbol(&mut self, c: char) -> bool {
        if self.peek() == &Token::Symbol(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.symbol(c) {
            Ok(())
        } else {
            Err(self.error(&format!("Expected {c}")))
        }
    }
    fn path(&mut self) -> Result<Plan, String> {
        let root = match self.take() {
            Token::Symbol('$') => true,
            Token::Symbol('@') => false,
            _ => return Err(self.error("Expected $ or @")),
        };
        let mut steps = Vec::new();
        loop {
            let recursive = if self.peek() == &Token::Descend {
                self.take();
                true
            } else {
                false
            };
            let dotted = recursive || self.symbol('.');
            let selectors = if self.symbol('[') {
                let mut selectors = Vec::new();
                loop {
                    selectors.push(self.selector()?);
                    if !self.symbol(',') {
                        break;
                    }
                    if selectors.len() >= 64 {
                        return Err(self.error("Union exceeds 64 selectors"));
                    }
                }
                self.expect(']')?;
                selectors
            } else if dotted {
                vec![match self.take() {
                    Token::Ident(s) => Selector::Name(s),
                    Token::Symbol('*') => Selector::Wildcard,
                    _ => return Err(self.error("Expected field name or *")),
                }]
            } else {
                break;
            };
            steps.push(Step {
                recursive,
                selectors,
            });
            if steps.len() > 64 {
                return Err(self.error("Query exceeds 64 segments"));
            }
        }
        Ok(Plan { root, steps })
    }
    fn integer(&mut self) -> Result<Option<i64>, String> {
        if let Token::Number(n) = self.peek() {
            let n = n
                .parse::<i64>()
                .map_err(|_| self.error("Array index/slice requires i64 integer"))?;
            self.take();
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }
    fn selector(&mut self) -> Result<Selector, String> {
        if let Token::Text(s) = self.peek() {
            let s = s.clone();
            self.take();
            return Ok(Selector::Name(s));
        }
        if self.symbol('*') {
            return Ok(Selector::Wildcard);
        }
        if self.symbol('?') {
            return Ok(Selector::Filter(self.expr()?));
        }
        let start = self.integer()?;
        if self.symbol(':') {
            let end = self.integer()?;
            let step = if self.symbol(':') {
                self.integer()?.unwrap_or(1)
            } else {
                1
            };
            return Ok(Selector::Slice(start, end, step));
        }
        start
            .map(Selector::Index)
            .ok_or_else(|| self.error("Unsupported selector"))
    }
    fn expr(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > 32 {
            return Err(self.error("Filter nesting exceeds 32"));
        }
        let mut e = self.and()?;
        while self.peek() == &Token::Or {
            self.take();
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        self.depth -= 1;
        Ok(e)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.atom()?;
        while self.peek() == &Token::And {
            self.take();
            e = Expr::And(Box::new(e), Box::new(self.atom()?));
        }
        Ok(e)
    }
    fn atom(&mut self) -> Result<Expr, String> {
        if self.symbol('!') {
            self.depth += 1;
            if self.depth > 32 {
                return Err(self.error("Filter nesting exceeds 32"));
            }
            let e = self.atom()?;
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(e)));
        }
        if self.symbol('(') {
            let e = self.expr()?;
            self.expect(')')?;
            return Ok(e);
        }
        let left = self.operand()?;
        let op = match self.peek() {
            Token::Eq => Op::Eq,
            Token::Ne => Op::Ne,
            Token::Symbol('<') => Op::Lt,
            Token::Le => Op::Le,
            Token::Symbol('>') => Op::Gt,
            Token::Ge => Op::Ge,
            _ => {
                return match left {
                    Operand::Path(p) => Ok(Expr::Exists(p)),
                    _ => Err(self.error("Literal requires comparison")),
                };
            }
        };
        self.take();
        let right = self.operand()?;
        for operand in [&left, &right] {
            if let Operand::Path(p) = operand
                && !p.singular()
            {
                return Err(self.error("Comparison requires singular paths"));
            }
        }
        Ok(Expr::Compare(left, op, right))
    }
    fn operand(&mut self) -> Result<Operand, String> {
        if matches!(self.peek(), Token::Symbol('$' | '@')) {
            return self.path().map(Operand::Path);
        }
        match self.take() {
            Token::Text(s) => Ok(Operand::Literal(serde_json::Value::String(s))),
            Token::Number(s) => Ok(Operand::Literal(
                serde_json::from_str(&s).map_err(|e| e.to_string())?,
            )),
            Token::Ident(s) if matches!(s.as_str(), "true" | "false" | "null") => {
                Ok(Operand::Literal(serde_json::from_str(&s).unwrap()))
            }
            _ => Err(self.error("Unsupported operand or function")),
        }
    }
}

fn validate(plan: &Plan, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("Query AST nesting exceeds 64".into());
    }
    for step in &plan.steps {
        for selector in &step.selectors {
            if let Selector::Filter(expr) = selector {
                validate_expr(expr, depth + 1)?;
            }
        }
    }
    Ok(())
}
fn validate_expr(expr: &Expr, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("Query AST nesting exceeds 64".into());
    }
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) => {
            validate_expr(a, depth + 1)?;
            validate_expr(b, depth + 1)?;
        }
        Expr::Not(e) => validate_expr(e, depth + 1)?,
        Expr::Exists(p) => validate(p, depth + 1)?,
        Expr::Compare(a, _, b) => {
            for o in [a, b] {
                if let Operand::Path(p) = o {
                    validate(p, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

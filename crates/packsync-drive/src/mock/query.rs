//! The subset of Drive's `q` language and `fields` selector the mock understands.
//!
//! `q`: `'ID' in parents`, `name = / != / contains '…'`, `mimeType = / != '…'`,
//! `trashed = / != true|false`, `appProperties has { key='k' and value='v' }`, combined with
//! `and`, `or`, `not` and parentheses. String literals use `\'` and `\\` escapes. Anything else is
//! rejected with an error (the mock answers `400 invalid`), like Drive does for bad queries.

use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    InParents(String),
    Name { op: Op, value: String },
    Mime { negate: bool, value: String },
    Trashed { negate: bool, value: bool },
    AppProp { key: String, value: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Ne,
    Contains,
}

/// What an expression is evaluated against.
pub(crate) struct Subject<'a> {
    pub name: &'a str,
    pub mime: &'a str,
    pub parents: &'a [String],
    pub trashed: bool,
    pub app_props: &'a BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Word(String),
    Sym(&'static str),
}

fn tokenize(q: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = q.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' | ')' | '{' | '}' | '=' => {
                out.push(Tok::Sym(match c {
                    '(' => "(",
                    ')' => ")",
                    '{' => "{",
                    '}' => "}",
                    _ => "=",
                }));
                i += 1;
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                out.push(Tok::Sym("!="));
                i += 2;
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("unterminated string literal".into()),
                        Some('\\') => {
                            let Some(n) = chars.get(i + 1) else { return Err("dangling escape".into()) };
                            s.push(*n);
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(ch) => {
                            s.push(*ch);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Word(chars[start..i].iter().collect()));
            }
            other => return Err(format!("unexpected character {other:?}")),
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }
    fn is_kw(t: Option<&Tok>, kw: &str) -> bool {
        matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }
    fn expect_kw(&mut self, kw: &str) -> Result<(), String> {
        if Self::is_kw(self.peek(), kw) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{kw}'"))
        }
    }
    fn expect_sym(&mut self, s: &str) -> Result<(), String> {
        match self.next() {
            Some(Tok::Sym(x)) if x == s => Ok(()),
            _ => Err(format!("expected '{s}'")),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            _ => Err("expected a string literal".into()),
        }
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut e = self.and()?;
        while Self::is_kw(self.peek(), "or") {
            self.pos += 1;
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        while Self::is_kw(self.peek(), "and") {
            self.pos += 1;
            e = Expr::And(Box::new(e), Box::new(self.unary()?));
        }
        Ok(e)
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if Self::is_kw(self.peek(), "not") {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.peek() == Some(&Tok::Sym("(")) {
            self.pos += 1;
            let e = self.or()?;
            self.expect_sym(")")?;
            return Ok(e);
        }
        self.predicate()
    }
    fn eq_or_ne(&mut self) -> Result<bool, String> {
        match self.next() {
            Some(Tok::Sym("=")) => Ok(false),
            Some(Tok::Sym("!=")) => Ok(true),
            _ => Err("expected '=' or '!='".into()),
        }
    }
    fn predicate(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Tok::Str(id)) => {
                self.expect_kw("in")?;
                self.expect_kw("parents")?;
                Ok(Expr::InParents(id))
            }
            Some(Tok::Word(w)) if w == "name" => {
                let op = match self.next() {
                    Some(Tok::Sym("=")) => Op::Eq,
                    Some(Tok::Sym("!=")) => Op::Ne,
                    Some(Tok::Word(c)) if c.eq_ignore_ascii_case("contains") => Op::Contains,
                    _ => return Err("bad operator after 'name'".into()),
                };
                Ok(Expr::Name { op, value: self.string()? })
            }
            Some(Tok::Word(w)) if w == "mimeType" => {
                let negate = self.eq_or_ne()?;
                Ok(Expr::Mime { negate, value: self.string()? })
            }
            Some(Tok::Word(w)) if w == "trashed" => {
                let negate = self.eq_or_ne()?;
                let value = match self.next() {
                    Some(Tok::Word(b)) if b.eq_ignore_ascii_case("true") => true,
                    Some(Tok::Word(b)) if b.eq_ignore_ascii_case("false") => false,
                    _ => return Err("expected true or false".into()),
                };
                Ok(Expr::Trashed { negate, value })
            }
            Some(Tok::Word(w)) if w == "appProperties" => {
                self.expect_kw("has")?;
                self.expect_sym("{")?;
                self.expect_kw("key")?;
                self.expect_sym("=")?;
                let key = self.string()?;
                self.expect_kw("and")?;
                self.expect_kw("value")?;
                self.expect_sym("=")?;
                let value = self.string()?;
                self.expect_sym("}")?;
                Ok(Expr::AppProp { key, value })
            }
            other => Err(format!("unsupported query term {other:?}")),
        }
    }
}

/// Parse a `q` expression.
pub(crate) fn parse(q: &str) -> Result<Expr, String> {
    let mut p = Parser { toks: tokenize(q)?, pos: 0 };
    let e = p.or()?;
    if p.pos != p.toks.len() {
        return Err("trailing input in query".into());
    }
    Ok(e)
}

/// Evaluate. `alias` maps `root` / `appDataFolder` to the mock's real IDs.
pub(crate) fn eval(e: &Expr, s: &Subject<'_>, alias: &dyn Fn(&str) -> String) -> bool {
    match e {
        Expr::And(a, b) => eval(a, s, alias) && eval(b, s, alias),
        Expr::Or(a, b) => eval(a, s, alias) || eval(b, s, alias),
        Expr::Not(a) => !eval(a, s, alias),
        Expr::InParents(id) => {
            let id = alias(id);
            s.parents.contains(&id)
        }
        Expr::Name { op: Op::Eq, value } => s.name == value,
        Expr::Name { op: Op::Ne, value } => s.name != value,
        Expr::Name { op: Op::Contains, value } => s.name.contains(value.as_str()),
        Expr::Mime { negate, value } => (s.mime == value) != *negate,
        Expr::Trashed { negate, value } => (s.trashed == *value) != *negate,
        Expr::AppProp { key, value } => s.app_props.get(key) == Some(value),
    }
}

/// A parsed `fields` selector (`a,b(c,d),files(id,name)`); `*` selects everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fields(pub Vec<(String, Option<Fields>)>);

pub(crate) fn parse_fields(s: &str) -> Result<Fields, String> {
    fn parse_list(chars: &[char], i: &mut usize, nested: bool) -> Result<Fields, String> {
        let mut out = Vec::new();
        loop {
            let start = *i;
            while *i < chars.len() && (chars[*i].is_ascii_alphanumeric() || chars[*i] == '*' || chars[*i] == '_') {
                *i += 1;
            }
            let name: String = chars[start..*i].iter().collect();
            if name.is_empty() {
                return Err("empty field name".into());
            }
            let sub = if chars.get(*i) == Some(&'(') {
                *i += 1;
                let f = parse_list(chars, i, true)?;
                if chars.get(*i) != Some(&')') {
                    return Err("unbalanced parentheses".into());
                }
                *i += 1;
                Some(f)
            } else {
                None
            };
            out.push((name, sub));
            match chars.get(*i) {
                Some(',') => *i += 1,
                Some(')') if nested => return Ok(Fields(out)),
                None if !nested => return Ok(Fields(out)),
                _ => return Err("malformed fields selector".into()),
            }
        }
    }
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut i = 0;
    parse_list(&chars, &mut i, false)
}

/// Keep only the selected members of `v` (recursing into nested selectors and arrays).
pub(crate) fn select(v: &Value, f: &Fields) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(|x| select(x, f)).collect()),
        Value::Object(o) => {
            if f.0.iter().any(|(n, _)| n == "*") {
                return v.clone();
            }
            let mut out = serde_json::Map::new();
            for (name, sub) in &f.0 {
                if let Some(x) = o.get(name) {
                    out.insert(
                        name.clone(),
                        match sub {
                            Some(sf) => select(x, sf),
                            None => x.clone(),
                        },
                    );
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject<'a>(props: &'a BTreeMap<String, String>, parents: &'a [String]) -> Subject<'a> {
        Subject { name: "it's a\\b ünï", mime: "application/octet-stream", parents, trashed: false, app_props: props }
    }

    #[test]
    fn parses_and_evaluates_client_queries() {
        let mut props = BTreeMap::new();
        props.insert("ps".to_string(), "1".to_string());
        let parents = vec!["P1".to_string()];
        let s = subject(&props, &parents);
        let id = |x: &str| x.to_string();
        let q = r"name = 'it\'s a\\b ünï' and trashed = false and mimeType != 'application/vnd.google-apps.folder' and appProperties has { key='ps' and value='1' } and ('P0' in parents or 'P1' in parents)";
        assert!(eval(&parse(q).unwrap(), &s, &id));
        assert!(!eval(&parse("name = 'other'").unwrap(), &s, &id));
        assert!(eval(&parse("not trashed = true and name contains 'ünï'").unwrap(), &s, &id));
        assert!(!eval(&parse("appProperties has { key='ps' and value='0' }").unwrap(), &s, &id));
        for bad in ["name = 'x", "name ~ 'x'", "foo = 'x'", "('a' in parents", "name = 'x' extra"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fields_selection() {
        let f = parse_fields("nextPageToken,files(id,name)").unwrap();
        let v =
            serde_json::json!({"nextPageToken": "t", "kind": "k", "files": [{"id": "1", "name": "a", "size": "3"}]});
        assert_eq!(select(&v, &f), serde_json::json!({"nextPageToken": "t", "files": [{"id": "1", "name": "a"}]}));
        assert_eq!(select(&v, &parse_fields("*").unwrap()), v);
        assert!(parse_fields("a,(b)").is_err());
        assert!(parse_fields("a(b").is_err());
    }
}

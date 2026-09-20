//! Bounded Boolean history grammar and set-based SQL compilation.
use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, SecondsFormat};
use rusqlite::types::Value;

#[derive(Clone, Debug)]
pub enum Expression {
    Leaf(String, String, usize),
    Repos(Vec<String>),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}
#[derive(Debug)]
struct Token {
    text: String,
    quoted: bool,
    pos: usize,
}
impl Token {
    fn is(&self, s: &str) -> bool {
        !self.quoted && self.text.eq_ignore_ascii_case(s)
    }
}

pub fn parse(input: &str) -> Result<Option<Expression>> {
    if input.len() > 4096 {
        bail!("query at byte 4096: maximum length is 4096 UTF-8 bytes");
    }
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let c = input[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let pos = i;
        let quoted = c == '"';
        let text = if quoted {
            i += 1;
            let mut escaped = false;
            let mut closed = false;
            while i < input.len() {
                let c = input.as_bytes()[i];
                i += 1;
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    closed = true;
                    break;
                }
            }
            if !closed {
                bail!("query at byte {pos}: unterminated quoted string");
            }
            serde_json::from_str::<String>(&input[pos..i])
                .with_context(|| format!("query at byte {pos}: invalid JSON string"))?
        } else if "():".contains(c) {
            i += 1;
            c.to_string()
        } else {
            while i < input.len() {
                let c = input[i..].chars().next().unwrap();
                if c.is_whitespace() || "():".contains(c) {
                    break;
                }
                i += c.len_utf8();
            }
            input[pos..i].to_owned()
        };
        if text.contains('\0') {
            bail!("query at byte {pos}: NUL is not allowed");
        }
        tokens.push(Token { text, quoted, pos });
        if tokens.len() > 256 {
            bail!("query at byte {pos}: maximum is 256 tokens");
        }
    }
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut p = Parser {
        tokens,
        i: 0,
        end: input.len(),
    };
    let expr = p.or(0)?;
    if p.i != p.tokens.len() {
        return p.error("unexpected token");
    }
    Ok(Some(expr))
}
struct Parser {
    tokens: Vec<Token>,
    i: usize,
    end: usize,
}
impl Parser {
    fn is(&self, s: &str) -> bool {
        self.tokens.get(self.i).is_some_and(|t| t.is(s))
    }
    fn error<T>(&self, message: &str) -> Result<T> {
        bail!(
            "query at byte {}: {message}",
            self.tokens.get(self.i).map_or(self.end, |t| t.pos)
        )
    }
    fn or(&mut self, depth: usize) -> Result<Expression> {
        let mut e = self.and(depth)?;
        while self.is("OR") {
            self.i += 1;
            e = Expression::Or(Box::new(e), Box::new(self.and(depth)?));
        }
        Ok(e)
    }
    fn and(&mut self, depth: usize) -> Result<Expression> {
        let mut e = self.unary(depth)?;
        while self.i < self.tokens.len() && !self.is("OR") && !self.is(")") {
            if self.is("AND") {
                self.i += 1;
            }
            e = Expression::And(Box::new(e), Box::new(self.unary(depth)?));
        }
        Ok(e)
    }
    fn unary(&mut self, depth: usize) -> Result<Expression> {
        if self.is("NOT") || self.is("(") {
            if depth >= 32 {
                return self.error("maximum nesting depth is 32");
            }
            let negated = self.is("NOT");
            self.i += 1;
            if negated {
                return Ok(Expression::Not(Box::new(self.unary(depth + 1)?)));
            }
            let e = self.or(depth + 1)?;
            if !self.is(")") {
                return self.error("expected closing parenthesis");
            }
            self.i += 1;
            return Ok(e);
        }
        if self.i == self.tokens.len() || ["AND", "OR", ")", ":"].iter().any(|s| self.is(s)) {
            return self.error("expected a term");
        }
        let t = &self.tokens[self.i];
        let (text, pos, quoted) = (t.text.clone(), t.pos, t.quoted);
        self.i += 1;
        if self.is(":") {
            if quoted {
                return self.error("field name must be unquoted");
            }
            let field = text.to_ascii_lowercase();
            match field.as_str() {
                "repo" | "view" | "bundle" | "since" | "until" => {}
                "match" => bail!("query at byte {pos}: use AND/OR instead of match:"),
                "context" => bail!(
                    "query at byte {pos}: Boolean selection keeps companions; legacy --repo/--view detail narrowing is controlled by --full-context, not context:"
                ),
                _ => bail!("query at byte {pos}: unknown field {text:?}"),
            }
            self.i += 1;
            if self.i == self.tokens.len() || ["(", ")", ":"].iter().any(|s| self.is(s)) {
                return self.error("expected nonempty field value");
            }
            let value = self.tokens[self.i].text.clone();
            if value.is_empty() {
                return self.error("expected nonempty field value");
            }
            self.i += 1;
            if field == "since" || field == "until" {
                date(&value, field == "until", pos)?;
            }
            Ok(Expression::Leaf(field, value, pos))
        } else {
            Ok(Expression::Leaf("text".into(), text, pos))
        }
    }
}
fn date(value: &str, until: bool, pos: usize) -> Result<String> {
    let fail = || format!("query at byte {pos}: date must be a valid YYYY-MM-DD");
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").with_context(fail)?;
    if value.starts_with("0000-")
        || value.len() != 10
        || date.format("%Y-%m-%d").to_string() != value
    {
        bail!("{}", fail());
    }
    // The exclusive next midnight would be year +10000, which does not
    // preserve RFC3339 lexical ordering. Use the inclusive last nanosecond.
    if until && value == "9999-12-31" {
        return Ok("9999-12-31T23:59:59.999999999Z".into());
    }
    let date = if until {
        date.succ_opt().context("date out of range")?
    } else {
        date
    };
    Ok(date
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339_opts(SecondsFormat::Nanos, true))
}
impl Expression {
    pub fn resolve_views(
        &mut self,
        resolve: &mut impl FnMut(&str) -> Result<Vec<String>>,
    ) -> Result<()> {
        match self {
            Self::Leaf(field, name, pos) if field == "view" => {
                let repos = resolve(name)
                    .with_context(|| format!("query at byte {pos}: resolving view {name:?}"))?;
                *self = Self::Repos(repos);
            }
            Self::Not(e) => e.resolve_views(resolve)?,
            Self::And(a, b) | Self::Or(a, b) => {
                a.resolve_views(resolve)?;
                b.resolve_views(resolve)?;
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn sql(&self, key: &str, bind: &mut impl FnMut(Value) -> String) -> Result<String> {
        Ok(match self {
            Self::Repos(repos) => {
                let parameter = bind(serde_json::to_string(repos)?.into());
                format!(
                    "{key} IN (SELECT {key} FROM events \
                     WHERE repo IN (SELECT value FROM json_each({parameter})))"
                )
            }
            Self::Not(expression) => {
                let operand = expression.sql(key, bind)?;
                format!("NOT ({operand})")
            }
            Self::And(left, right) | Self::Or(left, right) => {
                let operator = if matches!(self, Self::And(..)) {
                    "AND"
                } else {
                    "OR"
                };
                let left = left.sql(key, bind)?;
                let right = right.sql(key, bind)?;
                format!("({left}) {operator} ({right})")
            }
            Self::Leaf(field, value, pos) => {
                let condition = match field.as_str() {
                    "view" => bail!("query at byte {pos}: unresolved view {value:?}"),
                    "since" | "until" => {
                        let operator = if field == "since" {
                            ">="
                        } else if value == "9999-12-31" {
                            "<="
                        } else {
                            "<"
                        };
                        let boundary = date(value, field == "until", *pos)?;
                        let parameter = bind(boundary.into());
                        return Ok(format!(
                            "{key} IN (SELECT {key} FROM events GROUP BY {key} \
                             HAVING MAX(history_activity(payload)) {operator} {parameter})"
                        ));
                    }
                    "repo" | "bundle" => {
                        let parameter = bind(value.clone().into());
                        format!("{field}={parameter}")
                    }
                    "text" if value.is_empty() => "1".into(),
                    "text" => {
                        let parameter = bind(value.to_lowercase().into());
                        format!("history_contains(payload,{parameter})")
                    }
                    _ => unreachable!(),
                };
                format!("{key} IN (SELECT {key} FROM events WHERE {condition})")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_strings_and_keywords() {
        for good in [
            "repo:AND",
            "repo:OR",
            "repo:NOT",
            r#""quoted\\value\"and\u0061""#,
            "'literal'",
            "repo:API",
            "repo:api (repo:web OR NOT timeout)",
        ] {
            assert!(parse(good).is_ok(), "{good}");
        }
        for bad in [
            r#""bad\q""#,
            "since:2026-1-01",
            "repo:api:bad",
            "OR",
            "NOT",
            "repo:api AND",
            "repo:api OR OR",
            "since:0000-1-1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse(&"é".repeat(2049)).is_err());
        assert!(parse(&"x ".repeat(257)).is_err());
        assert!(parse(&"x ".repeat(256)).is_ok());
        assert!(parse(&format!("{}x{}", "(".repeat(32), ")".repeat(32))).is_ok());
        assert!(parse(&format!("{}x{}", "(".repeat(33), ")".repeat(33))).is_err());
        assert!(parse(&format!("{}x", "NOT ".repeat(33))).is_err());
    }
}

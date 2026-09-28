//! Git-style line matching with a deliberately bounded POSIX regex dialect.
//! Backreferences, collating/equivalence classes, locale collation and GNU
//! buffer anchors are not supported. Unsupported/ambiguous escapes are errors,
//! rather than being interpreted as Rust regex extensions. GNU word/space
//! escapes are accepted in BRE only: Git's ERE implementation varies by host.
//! Character matching/case folding uses Rust Unicode definitions, and POSIX
//! named bracket classes use ASCII definitions, independent of process locale.
use super::HistoryQuery;
use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};

pub(super) fn compile(pattern: &str, query: &HistoryQuery) -> Result<Regex> {
    // Git treats newlines in one -e/--grep argument as alternative patterns.
    let translated = pattern
        .split('\n')
        .map(|line| {
            if query.fixed_strings {
                Ok(regex::escape(line))
            } else {
                translate(line, query.extended_regexp)
            }
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|p| format!("(?:{p})"))
        .collect::<Vec<_>>()
        .join("|");
    RegexBuilder::new(&translated)
        .case_insensitive(query.ignore_case)
        .build()
        .with_context(|| format!("invalid history grep pattern {pattern:?}"))
}

fn translate(pattern: &str, extended: bool) -> Result<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut branch_start = true;
    let mut repeatable = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' {
            let start = i;
            i += 1;
            out.push('[');
            if chars.get(i) == Some(&'^') {
                out.push('^');
                i += 1;
            }
            if chars.get(i) == Some(&']') {
                out.push_str(r"\]");
                i += 1;
            }
            while i < chars.len() && chars[i] != ']' {
                match chars[i] {
                    '[' if chars.get(i + 1) == Some(&':') => {
                        let class_start = i;
                        i += 2;
                        while i < chars.len() && chars[i] != ':' {
                            i += 1;
                        }
                        if chars.get(i + 1) != Some(&']') {
                            bail!("invalid grep bracket class in {pattern:?}");
                        }
                        let class = chars[class_start + 2..i].iter().collect::<String>();
                        if !matches!(
                            class.as_str(),
                            "alnum"
                                | "alpha"
                                | "blank"
                                | "cntrl"
                                | "digit"
                                | "graph"
                                | "lower"
                                | "print"
                                | "punct"
                                | "space"
                                | "upper"
                                | "xdigit"
                        ) {
                            bail!("unsupported grep POSIX class {class:?}");
                        }
                        out.extend(chars[class_start..=i + 1].iter());
                        i += 2;
                    }
                    '[' if matches!(chars.get(i + 1), Some('.' | '=')) => {
                        bail!("grep collating/equivalence classes are unsupported")
                    }
                    '[' => {
                        out.push_str(r"\[");
                        i += 1;
                    }
                    '\\' => {
                        out.push_str(r"\\");
                        i += 1;
                    } // POSIX brackets treat backslash literally.
                    '&' | '~' => {
                        out.push('\\');
                        out.push(chars[i]);
                        i += 1;
                    }
                    '-' if chars.get(i + 1) == Some(&'-') => {
                        bail!("ambiguous grep bracket range in {pattern:?}")
                    }
                    ch => {
                        out.push(ch);
                        i += 1;
                    }
                }
            }
            if chars.get(i) != Some(&']') {
                bail!("unclosed grep bracket at character {start}");
            }
            out.push(']');
            i += 1;
            branch_start = false;
            repeatable = true;
            continue;
        }
        if c == '\\' {
            i += 1;
            let Some(&escaped) = chars.get(i) else {
                bail!("trailing backslash in grep pattern");
            };
            if extended && matches!(escaped, 'b' | 'B' | 'w' | 'W' | 's' | 'S' | '<' | '>') {
                bail!("grep word/space escapes in extended mode vary by Git platform; use basic mode or POSIX bracket classes");
            }
            match escaped {
                '1'..='9' => bail!(
                    "grep backreferences are unsupported; use a pattern without backreferences"
                ),
                'b' | 'B' | 'w' | 'W' | 's' | 'S' => {
                    out.push('\\');
                    out.push(escaped);
                }
                '<' => out.push_str(r"\b{start}"),
                '>' => out.push_str(r"\b{end}"),
                '(' | ')' | '|' | '+' | '?' if !extended => {
                    out.push(escaped);
                    branch_start = matches!(escaped, '(' | '|');
                    repeatable = !branch_start;
                    i += 1;
                    continue;
                }
                '{' if !extended => {
                    out.push('{');
                    i += 1;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == ',') {
                        out.push(chars[i]);
                        i += 1;
                    }
                    if chars.get(i) != Some(&'\\') || chars.get(i + 1) != Some(&'}') {
                        bail!("invalid grep repetition interval");
                    }
                    out.push('}');
                    i += 2;
                    branch_start = false;
                    continue;
                }
                ch if ch.is_ascii_alphanumeric() || matches!(ch, '`' | '\'') => {
                    bail!("unsupported grep escape \\{ch}")
                }
                ch => out.push_str(&regex::escape(&ch.to_string())),
            }
            i += 1;
            branch_start = false;
            repeatable = true;
            continue;
        }
        if extended && c == '(' && chars.get(i + 1) == Some(&'?') {
            bail!("grep supports POSIX groups, not (?...) extensions");
        }
        match c {
            '(' | ')' | '|' | '+' | '?' | '{' | '}' if !extended => {
                out.push_str(&regex::escape(&c.to_string()))
            }
            '^' if !extended && !branch_start => out.push_str(r"\^"),
            '$' if !extended
                && i + 1 < chars.len()
                && !(chars.get(i + 1) == Some(&'\\')
                    && matches!(chars.get(i + 2), Some(')' | '|'))) =>
            {
                out.push_str(r"\$")
            }
            '*' if !extended && !repeatable => out.push_str(r"\*"),
            ch => out.push(ch),
        }
        // In BRE a leading ^ may be followed by a literal leading *.
        repeatable = !(branch_start && c == '^' || extended && matches!(c, '(' | '|'));
        branch_start = extended && matches!(c, '(' | '|');
        i += 1;
    }
    Ok(out)
}

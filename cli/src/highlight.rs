//! A small Rhai lexer for colouring input and finding unfinished input.

use std::fmt::Write;

const KEYWORDS: &[&str] = &[
    "let", "const", "if", "else", "for", "in", "while", "loop", "do", "until", "break", "continue",
    "return", "throw", "try", "catch", "fn", "switch", "true", "false", "this", "print", "debug",
];

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    Keyword,
    Local,
    Number,
    String,
    Comment,
    Other,
}

/// Splits `code` into coloured spans. `locals` get their own colour.
pub fn spans<'a>(code: &'a str, locals: &[String]) -> Vec<(Kind, &'a str)> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < code.len() {
        let rest = &code[i..];
        let c = rest.chars().next().unwrap_or_default();
        let (kind, len) = if rest.starts_with("//") {
            (Kind::Comment, rest.find('\n').unwrap_or(rest.len()))
        } else if matches!(c, '"' | '`' | '\'') {
            (Kind::String, string_len(rest, c).unwrap_or(rest.len()))
        } else if c.is_ascii_digit() {
            (
                Kind::Number,
                rest.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                    .unwrap_or(rest.len()),
            )
        } else if c.is_alphabetic() || c == '_' {
            let len = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            let word = &rest[..len];
            let kind = if KEYWORDS.contains(&word) {
                Kind::Keyword
            } else if locals.iter().any(|l| l == word) && (i == 0 || bytes[i - 1] != b'.') {
                Kind::Local
            } else {
                Kind::Other
            };
            (kind, len)
        } else {
            (Kind::Other, c.len_utf8())
        };
        out.push((kind, &rest[..len]));
        i += len;
    }
    out
}

/// Length of the string literal at the start of `text`, `None` if open.
fn string_len(text: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return Some(i + c.len_utf8());
        }
    }
    None
}

/// Whether `code` still has open brackets or strings, so Enter should
/// continue on a new line.
pub fn incomplete(code: &str) -> bool {
    let mut depth = 0i32;
    for (kind, text) in spans(code, &[]) {
        match kind {
            Kind::String if string_len(text, text.chars().next().unwrap_or('"')).is_none() => {
                return true;
            }
            Kind::Other => {
                depth += match text {
                    "(" | "[" | "{" => 1,
                    ")" | "]" | "}" => -1,
                    _ => 0,
                };
            }
            _ => {}
        }
    }
    depth > 0
}

pub fn paint(code: &str, locals: &[String]) -> String {
    let mut out = String::new();
    for (kind, text) in spans(code, locals) {
        let color = match kind {
            Kind::Keyword => "35",
            Kind::Local => "1;33",
            Kind::Number => "36",
            Kind::String => "32",
            Kind::Comment => "2",
            Kind::Other => {
                out.push_str(text);
                continue;
            }
        };
        let _ = write!(out, "\x1b[{color}m{text}\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unfinished_input() {
        assert!(incomplete("for i in x {"));
        assert!(incomplete("print(\"a"));
        assert!(!incomplete("print(\"(\")"));
        assert!(!incomplete("x[0] // ("));
    }

    #[test]
    fn colours_locals_but_not_fields() {
        let locals = vec!["order".to_owned(), "id".to_owned()];
        let kinds: Vec<Kind> = spans("order.id + 1", &locals)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(kinds[0], Kind::Local);
        assert_eq!(kinds[2], Kind::Other);
        assert_eq!(kinds[6], Kind::Number);
    }
}

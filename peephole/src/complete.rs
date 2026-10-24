//! Completion and live previews, both from read-only path lookups.

use crate::node::{self, Node};
use crate::render;
use crate::session::Session;
use bevy_reflect::{PartialReflect, ReflectRef};
use rhai::Dynamic;
use serde_json::{Value, json};

const KEYWORDS: &[&str] = &[
    "let", "const", "if", "else", "for", "in", "while", "loop", "break", "continue", "return",
    "fn", "true", "false", "print", "debug",
];

/// Node methods from `node::register` that fit `value`.
fn methods(value: &dyn PartialReflect) -> Vec<(&'static str, &'static str)> {
    let mut out = vec![("type_name", "type_name()")];
    match value.reflect_ref() {
        ReflectRef::List(_) | ReflectRef::Array(_) | ReflectRef::Set(_) => out.extend([
            ("len", "len()"),
            ("to_array", "to_array()"),
            ("map", "map(|x| …)"),
            ("filter", "filter(|x| …)"),
        ]),
        ReflectRef::Map(_) => out.extend([
            ("len", "len()"),
            ("keys", "keys()"),
            ("to_array", "to_array()"),
            ("map", "map(|x| …)"),
            ("filter", "filter(|x| …)"),
        ]),
        ReflectRef::Struct(_) => out.push(("keys", "keys()")),
        ReflectRef::Enum(_) => out.push(("variant", "variant()")),
        _ => {}
    }
    out
}

enum Step<'a> {
    Member(&'a str),
    Index(i64),
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Parses `name(.member|[index]|["key"])*`.
fn parse(expr: &str) -> Option<(&str, Vec<Step<'_>>)> {
    let root_end = expr.find(|c: char| !is_ident(c)).unwrap_or(expr.len());
    let root = &expr[..root_end];
    if root.is_empty() || root.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut steps = Vec::new();
    let mut rest = &expr[root_end..];
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('.') {
            let end = tail.find(|c: char| !is_ident(c)).unwrap_or(tail.len());
            if end == 0 {
                return None;
            }
            steps.push(Step::Member(&tail[..end]));
            rest = &tail[end..];
        } else if let Some(tail) = rest.strip_prefix("[\"") {
            let end = tail.find("\"]")?;
            steps.push(Step::Member(&tail[..end]));
            rest = &tail[end + 2..];
        } else {
            let tail = rest.strip_prefix('[')?;
            let end = tail.find(']')?;
            steps.push(Step::Index(tail[..end].trim().parse().ok()?));
            rest = &tail[end + 1..];
        }
    }
    Some((root, steps))
}

/// Looks up a path without running code, so it has no side effects.
pub fn path(session: &Session, expr: &str) -> Option<Dynamic> {
    let (root, steps) = parse(expr.trim())?;
    let mut value = session.scope.get_value::<Dynamic>(root)?;
    for step in steps {
        let mut node = value.try_cast::<Node>()?;
        value = match step {
            Step::Member(name) => node::get_member(&mut node, name).ok()?,
            Step::Index(index) => node::get_element(&mut node, index).ok()?,
        };
    }
    Some(value)
}

/// The value of `text` if it is a plain path that goes past a variable.
pub fn preview(session: &Session, text: &str, width: usize) -> Option<String> {
    let text = text.trim();
    if !text.contains(['.', '[']) {
        return None;
    }
    path(session, text).map(|value| render::preview(&value, width))
}

/// The path being typed at the end of `line`.
fn trailing_expr(line: &str) -> &str {
    let mut depth = 0usize;
    let mut start = line.len();
    for (i, c) in line.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' if depth > 0 => depth -= 1,
            '"' if depth > 0 => {}
            c if is_ident(c) || c == '.' => {}
            _ if depth > 0 => {}
            _ => break,
        }
        start = i;
    }
    &line[start..]
}

fn exposed() -> Vec<(String, String)> {
    let functions = crate::FUNCTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    functions
        .iter()
        .map(|(name, function)| {
            // Paths of reference parameters start with `&` or `&mut `.
            let first = function
                .info()
                .base()
                .args()
                .first()
                .map_or("", |arg| arg.type_path());
            (
                name.clone(),
                first
                    .trim_start_matches('&')
                    .trim_start_matches("mut ")
                    .to_owned(),
            )
        })
        .collect()
}

pub fn complete(session: &Session, line: &str) -> Value {
    let expr = trailing_expr(line);
    let mut out: Vec<(String, String)> = Vec::new();
    let partial = if let Some(dot) = expr.rfind('.') {
        let partial = &expr[dot + 1..];
        if let Some(mut node) = path(session, &expr[..dot]).and_then(Dynamic::try_cast::<Node>)
            && let Ok(value) = node.get()
        {
            let type_path = value.reflect_type_path().to_owned();
            out.extend(
                node::members(value)
                    .into_iter()
                    .map(|(name, ty)| (name.clone(), format!("{name}: {ty}"))),
            );
            out.extend(
                methods(value)
                    .into_iter()
                    .map(|(name, shown)| (name.to_owned(), shown.to_owned())),
            );
            out.extend(
                exposed()
                    .into_iter()
                    .filter(|(_, first)| *first == type_path)
                    .map(|(name, _)| (name.clone(), format!("{name}()"))),
            );
        }
        partial
    } else {
        for var in &session.variables {
            out.push((
                var.name.to_owned(),
                format!("{}: {}", var.name, var.type_name),
            ));
        }
        for name in session.scope.iter_raw().map(|(name, ..)| name) {
            if !out.iter().any(|(known, _)| known == name) {
                out.push((name.to_owned(), name.to_owned()));
            }
        }
        out.extend(
            exposed()
                .into_iter()
                .map(|(name, _)| (name.clone(), format!("{name}()"))),
        );
        out.extend(KEYWORDS.iter().map(|k| ((*k).to_owned(), (*k).to_owned())));
        expr
    };
    out.retain(|(text, _)| text.starts_with(partial));
    out.dedup_by(|a, b| a.0 == b.0);
    let completions: Vec<Value> = out
        .into_iter()
        .map(|(text, display)| json!({"text": text, "display": display}))
        .collect();
    json!({"start": line.len() - partial.len(), "completions": completions})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_expression_being_typed() {
        assert_eq!(
            trailing_expr("let x = order.items[0].na"),
            "order.items[0].na"
        );
        assert_eq!(trailing_expr("f(names[\"bob\"].le"), "names[\"bob\"].le");
        assert_eq!(trailing_expr("1 + ord"), "ord");
    }

    #[test]
    fn parses_paths() {
        let (root, steps) = parse(r#"order.items[-1]["name"]"#).unwrap();
        assert_eq!(root, "order");
        assert!(matches!(
            steps.as_slice(),
            [Step::Member("items"), Step::Index(-1), Step::Member("name")]
        ));
        assert!(parse("order.total()").is_none());
        assert!(parse("1.5").is_none());
    }
}
